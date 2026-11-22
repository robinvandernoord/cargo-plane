//! `cargo plane setup container`: build the image on the host and create the
//! container.

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use cargo_plane_provider::{Host, Setup};
use serde_json::{Value, json};

use crate::outer::Outer;
use crate::{
    OWNER_LABEL, SPEC_LABEL, Spec, USERLAND_LABEL, check_engine, check_name, idle_policy,
    image_name, key_hash, key_path, read_public_key, volumes,
};

const USAGE: &str = "\
Usage: cargo plane [--remote <name>] setup container [host] [options]

Build a Docker or Podman image on an SSH host and create a build container
for this project from it. Builds run in the image's userland, so binaries
link against its glibc, not the host's.

Options:
  --host <host>          SSH hostname or ~/.ssh/config alias (same as the argument)
  --user <name>          SSH user (default: your SSH configuration)
  --port <port>          SSH port (default: your SSH configuration)
  --engine <engine>      docker or podman (default: detected on the host)
  --image <image>        Debian or Ubuntu base image (default: ubuntu:24.04);
                         pick the oldest glibc you need to run binaries on
  --packages <a,b,...>   extra apt packages, e.g. libssl-dev; repeatable
  --idle-timeout <min>   stop the container after this many minutes with
                         nothing running in it (default: never). Connections
                         stay open up to 10 minutes after cargo-plane's last
                         command, which adds to the time.
  --container <name>     container name (default: derived from the project)
  --rebuild              rebuild the image and recreate the container, even
                         if nothing changed; volumes (and so incremental
                         builds) are kept
  -y, --yes              don't ask; use defaults for anything not given

Safe to rerun: an existing container is reused unless its image, packages,
idle timeout or the local SSH key differ from what it was made with, or
--rebuild is given.
";

const DEFAULT_IMAGE: &str = "ubuntu:24.04";
const DOCKERFILE: &str = include_str!("../image/Dockerfile");
const ENTRYPOINT: &str = include_str!("../image/entrypoint.sh");

#[derive(Default)]
struct Options {
    host: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    engine: Option<String>,
    image: Option<String>,
    packages: Option<Vec<String>>,
    idle_minutes: Option<u32>,
    container: Option<String>,
    rebuild: bool,
    yes: bool,
}

pub fn setup(setup: &Setup, host: &dyn Host) -> Result<()> {
    let Some(opts) = parse_args(&setup.args)? else {
        print!("{USAGE}");
        return Ok(());
    };
    let ask_questions = !opts.yes && host.interactive();
    let project = setup.project_dir.as_deref().context(
        "run `cargo plane setup container` inside the project you want a build container for",
    )?;
    let config = setup.config.as_ref();
    let configured = |key: &str| config.and_then(|c| c.get(key));
    let configured_str = |key: &str| configured(key).and_then(Value::as_str).map(str::to_owned);

    // Where.
    let ssh_host = match opts.host.clone().or_else(|| configured_str("host")) {
        Some(name) if !ask_questions || opts.host.is_some() => name,
        existing if ask_questions => host.ask("SSH hostname or alias", existing.as_deref())?,
        _ => bail!(
            "provide a host with `cargo plane setup container <host>`, or run setup in a terminal"
        ),
    };
    let user = opts.user.clone().or_else(|| configured_str("user"));
    let port = opts.port.or_else(|| {
        configured("port")
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok())
    });

    host.status("Connecting", &format!("to {ssh_host}"));
    // Interactive the first time, so ssh can ask to trust the host key.
    let probe = Outer::new(&ssh_host, user.as_deref(), port, "docker")?;
    probe
        .run_visible("true", None, ask_questions)
        .with_context(|| format!("can't SSH to {ssh_host}; check that `ssh {ssh_host}` works"))?;
    // cargo-plane never prompts on later connections; a password login
    // would only work for the probe above.
    probe.run("true").with_context(|| {
        format!(
            "SSH to {ssh_host} needs a prompt (a password, say); cargo-plane needs \
             non-interactive logins, such as an SSH key loaded in ssh-agent"
        )
    })?;
    let engine = match opts.engine.clone().or_else(|| configured_str("engine")) {
        Some(engine) => engine,
        None => detect_engine(&probe)?,
    };
    let outer = Outer::new(&ssh_host, user.as_deref(), port, &engine)?;
    outer.check_access()?;

    // What.
    let container = match opts
        .container
        .clone()
        .or_else(|| configured_str("container"))
    {
        Some(name) => name,
        None => default_container_name(project, &setup.remote, &client_name()),
    };
    check_name(&container)?;
    check_retarget(
        host,
        &setup.remote,
        config,
        (&ssh_host, user.as_deref(), port, &engine, &container),
        opts.yes,
    )?;
    // Defaults for the questions below.
    let previous_image = configured_str("image").filter(|i| !i.is_empty());
    let previous_packages: Option<Vec<String>> = configured("packages")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        });
    let previous_idle = configured("idle_minutes")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);

    let image = match &opts.image {
        Some(image) => image.clone(),
        None => {
            let default = previous_image.as_deref().unwrap_or(DEFAULT_IMAGE);
            if ask_questions {
                host.ask("Base image (Debian or Ubuntu)", Some(default))?
            } else {
                default.to_owned()
            }
        }
    };
    check_image(&image)?;
    let idle_minutes = match opts.idle_minutes {
        Some(minutes) => minutes,
        None => {
            if ask_questions {
                ask_idle(host, previous_idle)?
            } else {
                previous_idle
            }
        }
    };
    let packages = opts
        .packages
        .clone()
        .or(previous_packages)
        .unwrap_or_default();
    // Before comparing specs: a new key pair has to get into a new image.
    ensure_key(host)?;
    let spec = Spec {
        image,
        packages,
        idle_minutes,
        key: key_hash(&read_public_key()?),
    };

    let found = outer.inspect(&container, SPEC_LABEL)?;
    let save = |spec: &Spec| {
        let mut update = json!({
            "host": ssh_host,
            "engine": engine,
            "container": container,
            "image": spec.image,
            "packages": spec.packages,
            "idle_minutes": spec.idle_minutes,
        });
        if let Some(user) = &user {
            update["user"] = json!(user);
        }
        if let Some(port) = port {
            update["port"] = json!(port);
        }
        host.save_config(&update)
    };

    let made = found.as_ref().and_then(Spec::from_label);
    // Another key means another machine made it (or this one lost its key):
    // replacing it would take over someone else's container and volumes.
    if found.is_some() && !opts.rebuild && made.as_ref().is_none_or(|m| m.key != spec.key) {
        bail!(
            "container {container} on {ssh_host} was made with a different SSH key: by \
             another machine, an older cargo-plane, or before this machine's key was lost. \
             Pass --rebuild to replace it, or --container <name> to make a separate one"
        );
    }
    // Reused only if the container itself was made from this spec; the
    // config may already describe a setup that failed partway.
    if let Some(found) = &found
        && !opts.rebuild
        && made.as_ref() == Some(&spec)
    {
        host.status(
            "Found",
            &format!(
                "container {container} on {ssh_host} ({}); use --rebuild to recreate it",
                found.state
            ),
        );
        return save(&spec);
    }

    let label = format!("container {container} on {ssh_host}");
    eprintln!("This will:");
    eprintln!(
        "  - build image {} on {ssh_host} with {engine}, from {}{}",
        image_name(&container),
        spec.image,
        if spec.packages.is_empty() {
            String::new()
        } else {
            format!(" plus {}", spec.packages.join(", "))
        }
    );
    // Cargo doesn't notice a new glibc or linker, so artifacts built in
    // another userland would count as fresh. The volume says which one it
    // holds builds for, whether or not the container that made it is still
    // around. The cargo caches are fine either way.
    let [(work_volume, _), ..] = volumes(&container);
    let work = match outer.volume_labels(&work_volume)? {
        None => Work::Create,
        Some(labels) => {
            let mine = labels.get(OWNER_LABEL) == Some(&spec.key);
            if !mine && !opts.rebuild {
                bail!(
                    "volume {work_volume} on {ssh_host} already exists, and wasn't made by \
                     cargo-plane on this machine (or was made by an older cargo-plane, or \
                     before this machine's key was lost). Pass --rebuild to empty it and use \
                     it, or --container <name> for a separate container and volumes"
                );
            }
            if mine && labels.get(USERLAND_LABEL) == Some(&spec.userland()) {
                Work::Keep
            } else {
                Work::Reset
            }
        }
    };
    let verb = if found.is_some() {
        "recreate"
    } else {
        "create"
    };
    match work {
        Work::Reset => eprintln!(
            "  - {verb} {label}, emptying its existing work volume (synced projects and \
             target/): it holds builds for another userland, or was made elsewhere; the \
             cargo cache is kept"
        ),
        Work::Keep if found.is_some() => eprintln!("  - recreate {label}, keeping its volumes"),
        Work::Keep => eprintln!("  - create {label}, reusing its existing volumes"),
        Work::Create => {
            eprintln!("  - {verb} {label} with volumes for builds and the cargo cache")
        }
    }
    eprintln!("  - {}", idle_policy(spec.idle_minutes));
    if !opts.yes {
        if !host.interactive() {
            bail!(
                "`cargo plane setup container` asks for confirmation; pass --yes, or run it in a terminal"
            );
        }
        if !host.confirm("Go ahead?", true)? {
            bail!("aborted");
        }
    }

    // Before creating anything, so `purge` can clean up a failed setup. A
    // rerun compares against the container's label, not this.
    save(&spec)?;
    create(&outer, host, &container, &spec, work).map_err(|err| {
        err.context(format!(
            "setup failed partway; rerun it to continue, or remove what was created with \
             `cargo plane --remote {} purge`",
            setup.remote
        ))
    })
}

fn create(outer: &Outer, host: &dyn Host, container: &str, spec: &Spec, work: Work) -> Result<()> {
    let image = image_name(container);
    host.status(
        "Building",
        &format!("image {image} on {} (slow the first time)", outer.host()),
    );
    let build = outer.engine_cmd(&[
        "build",
        "--tag",
        &image,
        "--build-arg",
        &format!("BASE={}", spec.image),
        "--build-arg",
        &format!("PACKAGES={}", spec.packages.join(" ")),
        "--build-arg",
        &format!("PUBKEY={}", read_public_key()?),
    ]);
    // An empty context with just the Dockerfile; `docker build -` would do,
    // but podman needs a directory.
    let script = format!(
        "set -e\ndir=$(mktemp -d)\ntrap 'rm -rf \"$dir\"' EXIT\ncat > \"$dir/Dockerfile\"\n{build} \"$dir\"\n"
    );
    outer.run_visible(&script, Some(&dockerfile()), false)?;

    host.status("Creating", &format!("container {container}"));
    let mut run = vec![
        "run".to_owned(),
        "--detach".into(),
        "--name".into(),
        container.into(),
        "--init".into(),
        "--restart".into(),
        "no".into(),
        "--env".into(),
        format!("IDLE_MINUTES={}", spec.idle_minutes),
        "--label".into(),
        format!("{SPEC_LABEL}={}", spec.label()),
    ];
    for (volume, path) in volumes(container) {
        run.extend(["--volume".into(), format!("{volume}:{path}")]);
    }
    run.push(image);
    let run: Vec<&str> = run.iter().map(String::as_str).collect();
    let mut script = format!(
        "set -e\n{} >/dev/null 2>&1 || true\n",
        outer.engine_cmd(&["rm", "--force", container]),
    );
    let [(work_volume, _), ..] = volumes(container);
    if work == Work::Reset {
        script += &format!(
            "{} >/dev/null\n",
            outer.engine_cmd(&["volume", "rm", &work_volume])
        );
    }
    if work != Work::Keep {
        // Created here rather than by `run`, to carry the labels.
        let userland = format!("{USERLAND_LABEL}={}", spec.userland());
        let owner = format!("{OWNER_LABEL}={}", spec.key);
        script += &format!(
            "{} >/dev/null\n",
            outer.engine_cmd(&[
                "volume",
                "create",
                "--label",
                &userland,
                "--label",
                &owner,
                &work_volume
            ])
        );
    }
    script += &format!("{} >/dev/null\n", outer.engine_cmd(&run));
    outer.run(&script)?;
    Ok(())
}

/// What to do with the work volume (synced projects and `target/`).
#[derive(Clone, Copy, PartialEq)]
enum Work {
    /// It holds builds for this userland, or nothing yet.
    Keep,
    /// It doesn't exist.
    Create,
    /// It holds builds for another userland (or doesn't say which).
    Reset,
}

/// Stop a rerun from quietly pointing a remote at another container: the old
/// one and its volumes would stay behind, and `land` and `purge` would no
/// longer know about them.
fn check_retarget(
    host: &dyn Host,
    remote: &str,
    config: Option<&Value>,
    new: (&str, Option<&str>, Option<u16>, &str, &str),
    yes: bool,
) -> Result<()> {
    let Some(config) = config else {
        return Ok(());
    };
    let get = |key: &str| config.get(key).and_then(Value::as_str);
    let Some(old_container) = get("container") else {
        return Ok(());
    };
    let old = (
        get("host").unwrap_or_default(),
        get("user"),
        config
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok()),
        get("engine").unwrap_or("docker"),
        old_container,
    );
    if old == new {
        return Ok(());
    }
    let message = format!(
        "remote `{remote}` uses container {} on {} ({}); this would point it at container {} \
         on {} ({}) instead. The old container and its volumes would stay behind, and \
         cargo-plane would forget about them. Delete them first with `cargo plane --remote \
         {remote} purge`, or set up the new one under another name with `cargo plane \
         --remote <name> setup container`",
        old.4, old.0, old.3, new.4, new.0, new.3
    );
    if yes || !host.interactive() {
        bail!("{message}");
    }
    host.warn(&message);
    if !host.confirm("Leave the old container behind and continue?", false)? {
        bail!("aborted");
    }
    Ok(())
}

/// The Dockerfile with the entrypoint script inlined, since the build has no
/// context to copy it from.
fn dockerfile() -> String {
    let script = serde_json::to_string(ENTRYPOINT).expect("a string serializes");
    DOCKERFILE.replace("@ENTRYPOINT_SCRIPT@", &script)
}

/// Create the shared key pair if it doesn't exist yet.
fn ensure_key(host: &dyn Host) -> Result<()> {
    let key = key_path()?;
    let public = key.with_extension("pub");
    if key.is_file() {
        if !public.is_file() {
            // Only the public half is lost: derive it, so containers that
            // authorize this key keep working.
            host.status("Restoring", &format!("public key {}", public.display()));
            let output = Command::new("ssh-keygen")
                .arg("-y")
                .arg("-f")
                .arg(&key)
                .output()
                .context("can't run ssh-keygen")?;
            if !output.status.success() {
                bail!(
                    "can't derive {} from {}: {}",
                    public.display(),
                    key.display(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            fs::write(&public, &output.stdout)
                .with_context(|| format!("writing {}", public.display()))?;
        }
        return Ok(());
    }
    let dir = key.parent().unwrap();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    host.status("Creating", &format!("SSH key {}", key.display()));
    let status = Command::new("ssh-keygen")
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "cargo-plane-container",
            "-f",
        ])
        .arg(&key)
        .status()
        .context("can't run ssh-keygen")?;
    if !status.success() {
        bail!("ssh-keygen failed ({status})");
    }
    Ok(())
}

fn detect_engine(outer: &Outer) -> Result<String> {
    let found = outer.run(
        "if command -v docker >/dev/null; then echo docker; \
         elif command -v podman >/dev/null; then echo podman; fi",
    )?;
    match found.trim() {
        "" => bail!(
            "neither docker nor podman is installed on {}; install one, or pass --engine",
            outer.host()
        ),
        engine => Ok(engine.to_owned()),
    }
}

fn ask_idle(host: &dyn Host, current: u32) -> Result<u32> {
    let default = match current {
        0 => "never".to_owned(),
        n => n.to_string(),
    };
    loop {
        let answer = host.ask(
            "Stop the container after how many idle minutes (or never)",
            Some(&default),
        )?;
        match parse_idle(&answer) {
            Some(minutes) => return Ok(minutes),
            None => host.warn("answer a whole number of minutes, or `never`"),
        }
    }
}

fn parse_idle(value: &str) -> Option<u32> {
    match value.trim() {
        "never" | "" => Some(0),
        n => n.parse().ok(),
    }
}

fn check_image(image: &str) -> Result<()> {
    if image.is_empty() || image.starts_with('-') || image.chars().any(char::is_whitespace) {
        bail!("invalid image `{image}`");
    }
    Ok(())
}

fn check_package(package: &str) -> Result<()> {
    let mut chars = package.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+-.:=".contains(c));
    if !valid {
        bail!("invalid apt package name `{package}`");
    }
    Ok(())
}

/// This machine's hostname, or empty if it can't be found.
fn client_name() -> String {
    Command::new("uname")
        .arg("-n")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// `cargo-plane-<project>-<hash>`: recognizable on the host, and different
/// for same-named projects in different places, for each named remote of one
/// project, and for each client machine (CI runners often share all of the
/// above), so none of them share a container.
fn default_container_name(project: &Path, remote: &str, client: &str) -> String {
    let dir = project
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let slug = dir
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let mut slug = if slug.is_empty() {
        "project".to_owned()
    } else {
        slug
    };
    slug.truncate(40);
    let identity = format!("{}\0{remote}\0{client}", project.to_string_lossy());
    let hash = cargo_plane_provider::stable_hash(identity.as_bytes());
    format!(
        "cargo-plane-{}-{:06x}",
        slug.trim_end_matches('-'),
        hash & 0xff_ffff
    )
}

fn parse_args(args: &[String]) -> Result<Option<Options>> {
    let mut opts = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| {
            args.next()
                .filter(|v| !v.is_empty() && !v.starts_with('-'))
                .cloned()
                .with_context(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-y" | "--yes" => opts.yes = true,
            "--rebuild" => opts.rebuild = true,
            "--host" => set_host(&mut opts, value("--host")?)?,
            "--user" => opts.user = Some(value("--user")?),
            "--port" => {
                let port: u16 = value("--port")?
                    .parse()
                    .context("--port must be a number between 1 and 65535")?;
                if port == 0 {
                    bail!("--port must be a number between 1 and 65535");
                }
                opts.port = Some(port);
            }
            "--engine" => {
                let engine = value("--engine")?;
                check_engine(&engine)?;
                opts.engine = Some(engine);
            }
            "--image" => {
                let image = value("--image")?;
                check_image(&image)?;
                opts.image = Some(image);
            }
            "--packages" => {
                let list = value("--packages")?;
                let packages = opts.packages.get_or_insert_with(Vec::new);
                for package in list.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    check_package(package)?;
                    if !packages.iter().any(|p| p == package) {
                        packages.push(package.to_owned());
                    }
                }
            }
            "--idle-timeout" => {
                let minutes = value("--idle-timeout")?;
                opts.idle_minutes = Some(parse_idle(&minutes).with_context(|| {
                    format!("--idle-timeout takes whole minutes or `never`, not `{minutes}`")
                })?);
            }
            "--container" => {
                let name = value("--container")?;
                check_name(&name)?;
                opts.container = Some(name);
            }
            name if !name.starts_with('-') => set_host(&mut opts, name.to_owned())?,
            other => bail!("unexpected argument `{other}` for `setup container`\n\n{USAGE}"),
        }
    }
    Ok(Some(opts))
}

fn set_host(opts: &mut Options, host: String) -> Result<()> {
    if opts.host.is_some() {
        bail!("provide the SSH hostname only once, as an argument or with --host");
    }
    if host.trim().is_empty() {
        bail!("provide a nonempty SSH hostname or alias");
    }
    opts.host = Some(host);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Options>> {
        parse_args(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_options() {
        let opts = parse(&[
            "framework",
            "--image",
            "ubuntu:22.04",
            "--packages",
            "libssl-dev, protobuf-compiler",
            "--packages",
            "libssl-dev",
            "--idle-timeout",
            "20",
            "-y",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(opts.host.as_deref(), Some("framework"));
        assert_eq!(opts.image.as_deref(), Some("ubuntu:22.04"));
        assert_eq!(opts.packages.unwrap(), ["libssl-dev", "protobuf-compiler"]);
        assert_eq!(opts.idle_minutes, Some(20));
        assert!(opts.yes);
    }

    #[test]
    fn idle_timeout_never() {
        let opts = parse(&["--idle-timeout", "never"]).unwrap().unwrap();
        assert_eq!(opts.idle_minutes, Some(0));
        assert!(parse(&["--idle-timeout", "soon"]).is_err());
    }

    #[test]
    fn rejects_bad_arguments() {
        assert!(parse(&["a", "b"]).is_err());
        assert!(parse(&["--packages", "libssl-dev;rm"]).is_err());
        assert!(parse(&["--engine", "lxc"]).is_err());
        assert!(parse(&["--container", "Has Spaces"]).is_err());
        assert!(parse(&["--port", "0"]).is_err());
        assert!(parse(&["--frobnicate"]).is_err());
        assert!(parse(&["--help"]).unwrap().is_none());
    }

    #[test]
    fn container_names() {
        let project = Path::new("/home/me/My Project!");
        let name = default_container_name(project, "container", "laptop");
        assert!(name.starts_with("cargo-plane-my-project-"), "{name}");
        assert!(check_name(&name).is_ok());
        assert_eq!(name, default_container_name(project, "container", "laptop"));
        let elsewhere = Path::new("/elsewhere/My Project!");
        assert_ne!(
            name,
            default_container_name(elsewhere, "container", "laptop")
        );
        assert_ne!(name, default_container_name(project, "old-glibc", "laptop"));
        assert_ne!(
            name,
            default_container_name(project, "container", "ci-runner")
        );
    }

    #[test]
    fn dockerfile_inlines_entrypoint() {
        let dockerfile = dockerfile();
        assert!(!dockerfile.contains("@ENTRYPOINT_SCRIPT@"));
        let line = dockerfile
            .lines()
            .find(|l| l.starts_with("ENTRYPOINT"))
            .unwrap();
        let args: Vec<String> =
            serde_json::from_str(line.trim_start_matches("ENTRYPOINT ")).unwrap();
        assert_eq!(args[0], "/bin/sh");
        assert_eq!(args[2], ENTRYPOINT);
    }
}
