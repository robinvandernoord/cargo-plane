//! A Docker or Podman container on an SSH host: the host's CPU, the image's
//! userland. Binaries built in it link against the image's glibc instead of
//! the host's, so a newer host can build for an older laptop.
//! `cargo plane setup container` builds the image and creates the container.
//!
//! The container runs no SSH server and publishes no ports. Each connection
//! goes through a ProxyCommand: SSH to the host and run a one-shot `sshd -i`
//! in the container with `docker exec -i`, which gives cargo-plane's ssh and
//! rsync a normal SSH session. The ProxyCommand never starts the container,
//! so a status check doesn't undo an idle stop: once the container is
//! stopped, a cached connection fails fast and cargo-plane falls back to
//! `up()`, which starts it.

mod outer;
mod setup;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cargo_plane_provider::{Endpoint, Host, Provider, Setup, stable_hash};
use serde::{Deserialize, Serialize};

use self::outer::{Inspected, Outer};

#[derive(Deserialize)]
pub struct Config {
    /// SSH hostname or `~/.ssh/config` alias of the machine running the engine.
    host: String,
    user: Option<String>,
    port: Option<u16>,
    /// `docker` or `podman`.
    #[serde(default = "default_engine")]
    engine: String,
    /// The container's name on the host; also names its image and volumes.
    container: String,
    /// Base image the build image was made from, for `status` and setup.
    image: Option<String>,
    /// Extra apt packages in the image, e.g. `libssl-dev`.
    #[serde(default)]
    packages: Vec<String>,
    /// Stop the container after this many minutes with nothing running in it;
    /// 0 (the default) never stops it.
    #[serde(default)]
    idle_minutes: u32,
}

fn default_engine() -> String {
    "docker".into()
}

impl Config {
    /// The host, after checking the config: it may have been edited by
    /// hand, and its names end up in shell commands.
    fn outer(&self) -> Result<Outer> {
        check_name(&self.container)?;
        Outer::new(&self.host, self.user.as_deref(), self.port, &self.engine)
    }

    fn label(&self) -> String {
        format!("container {} on {}", self.container, self.host)
    }
}

/// What a container was made from, stored in its `SPEC_LABEL` as JSON. Setup
/// compares it with what's asked for, rather than with the saved config,
/// which can be ahead of the container after a failed setup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Spec {
    image: String,
    packages: Vec<String>,
    idle_minutes: u32,
    /// `stable_hash` of the authorized public key, so a lost key pair
    /// leads to a rebuild with the new one.
    key: String,
}

const SPEC_LABEL: &str = "dev.cargo-plane.spec";

/// On the work volume: `Spec::userland` of the image whose builds it holds.
const USERLAND_LABEL: &str = "dev.cargo-plane.userland";

/// On the work volume: `Spec::key` of the machine that made it. Setup only
/// empties a volume it made itself, unless told to with `--rebuild`.
const OWNER_LABEL: &str = "dev.cargo-plane.owner";

impl Spec {
    fn from_label(inspected: &Inspected) -> Option<Self> {
        serde_json::from_str(inspected.label.as_deref()?).ok()
    }

    fn label(&self) -> String {
        serde_json::to_string(self).expect("a spec serializes")
    }

    /// What builds link against: the base image and packages, hashed.
    fn userland(&self) -> String {
        let id = serde_json::to_string(&(&self.image, &self.packages)).expect("serializes");
        format!("{:016x}", stable_hash(id.as_bytes()))
    }
}

fn key_hash(public_key: &str) -> String {
    format!("{:016x}", stable_hash(public_key.trim().as_bytes()))
}

/// Synced projects; a volume, so builds stay incremental across restarts
/// and image rebuilds.
const REMOTE_DIR: &str = "/work";

/// Volumes of a container, with where they're mounted.
fn volumes(container: &str) -> [(String, &'static str); 3] {
    [
        (format!("{container}-work"), REMOTE_DIR),
        (format!("{container}-registry"), "/root/.cargo/registry"),
        (format!("{container}-git"), "/root/.cargo/git"),
    ]
}

fn image_name(container: &str) -> String {
    format!("cargo-plane/{container}")
}

/// The key pair the image authorizes, shared by all containers made here.
fn key_path() -> Result<PathBuf> {
    Ok(cargo_plane_provider::data_dir()?
        .join("container")
        .join("id_ed25519"))
}

fn idle_policy(minutes: u32) -> String {
    match minutes {
        0 => "no auto-stop".into(),
        1 => "stops after 1 idle minute".into(),
        n => format!("stops after {n} idle minutes"),
    }
}

pub struct Container;

impl Provider for Container {
    type Config = Config;

    fn up(&self, config: &Config, host: &dyn Host) -> Result<Endpoint> {
        let outer = config.outer()?;
        let name = &config.container;
        let key = key_path()?;
        if !key.is_file() {
            bail!(
                "SSH key {} is missing; run `cargo plane setup container --rebuild` to make \
                 a new one and rebuild the container with it",
                key.display()
            );
        }
        match outer.inspect(name, SPEC_LABEL)? {
            None => bail!(
                "{} doesn't exist; run `cargo plane setup container` to create it",
                config.label()
            ),
            Some(found) if found.state == "running" => {}
            Some(found) => {
                host.status("Starting", &format!("{} ({})", config.label(), found.state));
                outer.run(&outer.engine_cmd(&["start", name]))?;
            }
        }
        Ok(Endpoint {
            user: Some("root".into()),
            ssh_options: endpoint_options(&outer, name, &key),
            remote_dir: Some(REMOTE_DIR.into()),
            ..Endpoint::new(endpoint_host(&outer, name))
        })
    }

    fn down(&self, config: &Config, host: &dyn Host) -> Result<()> {
        let outer = config.outer()?;
        let name = &config.container;
        if outer
            .inspect(name, SPEC_LABEL)?
            .is_some_and(|found| found.state == "running")
        {
            host.status("Stopping", &config.label());
            outer.run(&outer.engine_cmd(&["stop", name]))?;
        }
        Ok(())
    }

    fn status(&self, config: &Config, _: &dyn Host) -> Result<String> {
        let outer = config.outer()?;
        let Some(found) = outer.inspect(&config.container, SPEC_LABEL)? else {
            return Ok(format!(
                "{} doesn't exist (run `cargo plane setup container`)",
                config.label()
            ));
        };
        // What the container really has; the config can be ahead of it.
        let (image, packages, idle_minutes) = match Spec::from_label(&found) {
            Some(spec) => (spec.image, spec.packages, spec.idle_minutes),
            None => (
                config
                    .image
                    .clone()
                    .unwrap_or_else(|| "unknown base image".into()),
                config.packages.clone(),
                config.idle_minutes,
            ),
        };
        let mut base = image;
        if !packages.is_empty() {
            base = format!("{base} + {}", packages.join(", "));
        }
        Ok(format!(
            "{} ({}, {base}): {}, {}",
            config.label(),
            config.engine,
            found.state,
            idle_policy(idle_minutes)
        ))
    }

    fn purge(&self, config: &Config, host: &dyn Host) -> Result<bool> {
        if !host.interactive() {
            bail!("`cargo plane purge` asks for confirmation; run it in a terminal");
        }
        let outer = config.outer()?;
        // Otherwise an unreachable engine would look like nothing to delete.
        outer.check_access()?;
        let name = &config.container;
        let found = existing_resources(&outer, name)?;
        let found: Vec<_> = found
            .iter()
            .map(|(kind, name, _)| format!("{kind} {name}"))
            .collect();
        if found.is_empty() {
            host.status(
                "Gone",
                &format!(
                    "{} and its image and volumes don't exist (anymore)",
                    config.label()
                ),
            );
            return Ok(true);
        }
        eprintln!("This permanently deletes, on {}:", config.host);
        for resource in &found {
            eprintln!("  - {resource}");
        }
        eprintln!("The volumes hold synced projects, target directories and the cargo cache.");
        if host.ask("Type the container name to confirm", None)? != *name {
            return Ok(false);
        }
        host.status("Deleting", &config.label());
        // Fails on anything but a missing resource, so the config (which
        // cargo-plane removes after a successful purge) survives for a retry.
        remove_resources(&outer, name)
            .context("couldn't delete everything; fix the error and run purge again")?;
        Ok(true)
    }

    fn boot_timeout(&self, _: &Config) -> Duration {
        // `up()` has started the container; sshd only needs the exec to begin.
        Duration::from_secs(30)
    }

    fn setup(&self, setup: &Setup, host: &dyn Host) -> Result<()> {
        setup::setup(setup, host)
    }
}

/// The SSH destination cargo-plane connects to. Only the ProxyCommand uses
/// it, but OpenSSH names control sockets after it (`%C` ignores the
/// ProxyCommand), so it must differ for containers on different hosts or
/// engines, even if they have the same name.
fn endpoint_host(outer: &Outer, container: &str) -> String {
    let identity = format!("{}\0{}", outer.destination(), outer.engine_cmd(&[]));
    format!(
        "container.{container}.{:08x}",
        stable_hash(identity.as_bytes()) as u32
    )
}

/// SSH options that reach `sshd -i` inside the container through the host.
fn endpoint_options(outer: &Outer, container: &str, key: &std::path::Path) -> Vec<String> {
    let exec = outer.engine_cmd(&["exec", "-i", container, "/usr/sbin/sshd", "-i", "-e"]);
    let mut options = vec![
        // `-i`, not `-o IdentityFile=`: ssh parses `-o` values as config
        // lines, where a path with spaces would need its own quoting.
        "-i".to_owned(),
        key.display().to_string(),
    ];
    options.extend(
        [
            format!(
                "ProxyCommand={}",
                outer.proxy_command(&format!("exec {exec}"))
            ),
            "IdentitiesOnly=yes".into(),
            // The host is verified on the outer hop; the container's host
            // keys change with every image rebuild and aren't worth pinning.
            "StrictHostKeyChecking=no".into(),
            "UserKnownHostsFile=/dev/null".into(),
            "LogLevel=ERROR".into(),
        ]
        .into_iter()
        .flat_map(|option| ["-o".to_owned(), option]),
    );
    options
}

/// The container, its volumes and its image, as `(kind, name, rm flags)`.
/// A container may be running, so it's removed with force; volumes and
/// images aren't, so one that's still in use is an error, not a surprise.
fn resources(container: &str) -> Vec<(&'static str, String, &'static [&'static str])> {
    let mut resources = vec![("container", container.to_owned(), &["--force"][..])];
    resources.extend(
        volumes(container)
            .into_iter()
            .map(|(volume, _)| ("volume", volume, &[][..])),
    );
    resources.push(("image", image_name(container), &[]));
    resources
}

/// Which of the container, its volumes and its image exist. Only objects
/// the engine says are missing are left out; any other failure is an error.
fn existing_resources(
    outer: &Outer,
    container: &str,
) -> Result<Vec<(&'static str, String, &'static [&'static str])>> {
    let mut found = Vec::new();
    for resource in resources(container) {
        if outer.exists(resource.0, &resource.1)? {
            found.push(resource);
        }
    }
    Ok(found)
}

/// Removes the container, its volumes and its image, stopping at the first
/// failure. Missing ones are skipped.
fn remove_resources(outer: &Outer, container: &str) -> Result<()> {
    for (kind, name, flags) in existing_resources(outer, container)? {
        let mut args = vec![kind, "rm"];
        args.extend(flags);
        args.push(&name);
        outer.run(&format!("{} >/dev/null", outer.engine_cmd(&args)))?;
    }
    Ok(())
}

/// Container names are also used for the image (`cargo-plane/<name>`) and
/// volumes, so they follow the strictest rules: a path component of an image
/// name. That's lowercase letters and digits, separated by `.`, `_`, `__` or
/// any number of `-`, starting and ending with a letter or digit.
fn check_name(name: &str) -> Result<()> {
    let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let mut valid =
        !name.is_empty() && name.len() <= 63 && name.starts_with(alnum) && name.ends_with(alnum);
    // Each run of separators between letters and digits.
    for separator in name.split(alnum).filter(|s| !s.is_empty()) {
        valid &= matches!(separator, "." | "_" | "__") || separator.chars().all(|c| c == '-');
    }
    if !valid {
        bail!(
            "invalid container name `{name}`: use up to 63 lowercase letters and digits, \
             separated by `.`, `_`, `__` or dashes, starting and ending with a letter or digit"
        );
    }
    Ok(())
}

fn check_engine(engine: &str) -> Result<()> {
    match engine {
        "docker" | "podman" => Ok(()),
        other => bail!("unsupported engine `{other}`; use `docker` or `podman`"),
    }
}

fn read_public_key() -> Result<String> {
    // Created by setup's `ensure_key`.
    let path = key_path()?.with_extension("pub");
    let key =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(key.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(check_name("cargo-plane-foo-1a2b3c").is_ok());
        assert!(check_name("a").is_ok());
        assert!(check_name("").is_err());
        assert!(check_name("-foo").is_err());
        assert!(check_name("Foo").is_err());
        assert!(check_name("foo bar").is_err());
        assert!(check_name(&"a".repeat(64)).is_err());
        assert!(check_name("a--b").is_ok());
        assert!(check_name("a__b.c_d").is_ok());
        assert!(check_name("build-").is_err());
        assert!(check_name("build..box").is_err());
        assert!(check_name("a___b").is_err());
        assert!(check_name("a.-b").is_err());
        assert!(check_name("a;rm -rf x").is_err());
    }

    #[test]
    fn proxy_command_reaches_sshd() {
        let outer = Outer::new("framework", Some("robin"), Some(2222), "docker").unwrap();
        let options = endpoint_options(&outer, "box", std::path::Path::new("/k/id"));
        assert_eq!(options[..2], ["-i", "/k/id"]);
        assert_eq!(
            options[3],
            "ProxyCommand=ssh -T -o BatchMode=yes -p 2222 -l robin -- framework \
             \"exec docker exec -i box /usr/sbin/sshd -i -e\""
        );
    }

    #[test]
    fn endpoint_host_depends_on_destination() {
        let host = |h: &str, user: Option<&str>, port: Option<u16>, engine: &str| {
            endpoint_host(&Outer::new(h, user, port, engine).unwrap(), "box")
        };
        let base = host("framework", None, None, "docker");
        assert!(base.starts_with("container.box."), "{base}");
        assert_eq!(base, host("framework", None, None, "docker"));
        assert_ne!(base, host("other", None, None, "docker"));
        assert_ne!(base, host("framework", Some("me"), None, "docker"));
        assert_ne!(base, host("framework", None, Some(2222), "docker"));
        assert_ne!(base, host("framework", None, None, "podman"));
    }

    #[test]
    fn spec_label_round_trips() {
        let spec = Spec {
            image: "ubuntu:22.04".into(),
            packages: vec!["libssl-dev".into()],
            idle_minutes: 5,
            key: key_hash("ssh-ed25519 AAAA test\n"),
        };
        let found = |label: Option<String>| Inspected {
            state: "running".into(),
            label,
        };
        assert_eq!(Spec::from_label(&found(Some(spec.label()))), Some(spec));
        assert_eq!(Spec::from_label(&found(None)), None);
        assert_eq!(Spec::from_label(&found(Some("junk".into()))), None);
    }

    #[test]
    fn resources_of_a_container() {
        let names: Vec<_> = resources("box")
            .into_iter()
            .map(|(kind, name, flags)| format!("{kind} {name} {flags:?}"))
            .collect();
        assert_eq!(
            names,
            [
                r#"container box ["--force"]"#,
                "volume box-work []",
                "volume box-registry []",
                "volume box-git []",
                "image cargo-plane/box []",
            ]
        );
    }

    #[test]
    fn idle_policies() {
        assert_eq!(idle_policy(0), "no auto-stop");
        assert_eq!(idle_policy(1), "stops after 1 idle minute");
        assert_eq!(idle_policy(20), "stops after 20 idle minutes");
    }
}
