//! `cargo plane setup fly`: install flyctl, log in, create the app, volume and
//! builder machine, and add the remote to the global config.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::flyctl::{self, Flyctl, METADATA_KEY, METADATA_VALUE};
use crate::config::{Config, PROJECT_FILE};
use crate::project::workspace_root;
use crate::remote::Remote;
use crate::{ui, util};

const DOCKERFILE: &str = include_str!("../../../contrib/fly/Dockerfile");
const ENTRYPOINT: &str = include_str!("../../../contrib/fly/entrypoint.sh");
const VOLUME: &str = "cargo_plane_data";
const MACHINE_NAME: &str = "cargo-plane-builder";
const DEFAULT_VM_SIZE: &str = "performance-4x";

const USAGE: &str = "\
Usage: cargo plane [--remote <name>] setup fly [options]

Interactively installs flyctl if needed, logs in to Fly.io, creates an app,
a volume and a throwaway builder machine for the current project, and adds
the remote (default name: fly) to the project's .cargo/plane.toml. Safe to
rerun: existing parts are reused. Delete it all with `cargo plane purge fly`.

Options:
  --rebuild             recreate the machine (new image or settings); the volume is kept
  --dockerfile <path>   build from your own Dockerfile (see --eject)
  --eject <dir>         write the default Dockerfile and entrypoint to <dir> and exit
";

struct Options {
    rebuild: bool,
    dockerfile: Option<PathBuf>,
    eject: Option<PathBuf>,
}

pub fn setup(remote_name: Option<&str>, args: &[String]) -> Result<()> {
    let Some(opts) = parse_args(args)? else {
        print!("{USAGE}");
        return Ok(());
    };
    if let Some(dir) = &opts.eject {
        write_build_files(dir)?;
        ui::status(
            "Wrote",
            format!("Dockerfile and entrypoint.sh to {}", dir.display()),
        );
        return Ok(());
    }
    if !ui::interactive() {
        bail!("`cargo plane setup fly` asks questions; run it in a terminal");
    }
    let remote_name = remote_name.unwrap_or("fly");

    let fly = ensure_flyctl()?;
    ensure_login(&fly)?;

    // One throwaway machine per project, configured in that project.
    let root = workspace_root(None)
        .context("run `cargo plane setup fly` inside the project you want a build machine for")?;
    let config_path = root.join(PROJECT_FILE);
    let configured_app = configured_app(&config_path, remote_name)?;
    let project = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let default_app = configured_app
        .clone()
        .unwrap_or_else(|| default_app_name(project, &std::env::var("USER").unwrap_or_default()));
    let app = ui::ask("App name (globally unique on Fly.io)", Some(&default_app))?;

    let app_exists = fly.app_exists(&app);
    let builder = if app_exists {
        fly.builder(&app, None)?
    } else {
        None
    };

    match &builder {
        Some(machine) if !opts.rebuild => {
            ui::status(
                "Found",
                format!(
                    "builder machine {} in {app}; use --rebuild to recreate it",
                    machine.id
                ),
            );
            write_config(
                &config_path,
                remote_name,
                &app,
                configured_app.as_deref(),
                None,
            )?;
        }
        _ => {
            let plan = ask_plan(&fly, &app, app_exists)?;
            if !ui::confirm("Create these resources?", false)? {
                bail!("aborted");
            }
            // Before creating anything, so `purge` can clean up a failed setup.
            write_config(
                &config_path,
                remote_name,
                &app,
                configured_app.as_deref(),
                Some(&plan.ssh_key),
            )?;
            create(
                &fly,
                &app,
                &plan,
                builder.as_ref().map(|m| m.id.as_str()),
                &opts,
            )
            .map_err(|err| {
                err.context(format!(
                    "setup failed partway; rerun it to continue, or remove what was \
                     created with `cargo plane --remote {remote_name} purge`"
                ))
            })?;
        }
    }

    smoke_test(remote_name)
}

fn parse_args(args: &[String]) -> Result<Option<Options>> {
    let mut opts = Options {
        rebuild: false,
        dockerfile: None,
        eject: None,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--rebuild" => opts.rebuild = true,
            "--dockerfile" => {
                opts.dockerfile = Some(iter.next().context("--dockerfile needs a path")?.into())
            }
            "--eject" => {
                opts.eject = Some(iter.next().context("--eject needs a directory")?.into())
            }
            "-h" | "--help" => return Ok(None),
            other => bail!("unknown option `{other}` for `setup fly`\n\n{USAGE}"),
        }
    }
    Ok(Some(opts))
}

fn ensure_flyctl() -> Result<Flyctl> {
    if let Some(fly) = Flyctl::locate(None) {
        ui::status("Found", format!("flyctl at {}", fly.path().display()));
        return Ok(fly);
    }
    eprintln!("cargo-plane manages Fly.io machines through flyctl, which isn't installed.");
    let question = format!(
        "Download the latest flyctl from GitHub into {}?",
        flyctl::install_dir()?.display()
    );
    if !ui::confirm(&question, true)? {
        bail!(
            "flyctl is required; install it yourself (https://fly.io/docs/flyctl/install/) and rerun"
        );
    }
    flyctl::install()
}

fn ensure_login(fly: &Flyctl) -> Result<()> {
    if !fly.logged_in() {
        eprintln!(
            "\nYou're not logged in to Fly.io. flyctl will now open a browser to log in\n\
             (or sign up). Come back here when it's done.\n"
        );
        fly.interactive(&["auth", "login"])?;
        if !fly.logged_in() {
            bail!("still not logged in to Fly.io");
        }
    }
    let who = fly.output(&["auth", "whoami"])?;
    ui::status("Logged in", format!("as {}", who.trim()));
    Ok(())
}

struct Plan {
    org: Option<String>,
    region: String,
    vm_size: String,
    /// Overrides the size's default memory.
    memory_mb: Option<u64>,
    volume_gb: u32,
    idle_minutes: u32,
    ssh_key: PathBuf,
    /// The volume already exists (in `region`).
    volume_exists: bool,
}

fn ask_plan(fly: &Flyctl, app: &str, app_exists: bool) -> Result<Plan> {
    let org = if app_exists {
        None
    } else {
        Some(ask_org(fly)?)
    };

    let existing_volume = if app_exists {
        find_volume(fly, app)?
    } else {
        None
    };
    let region = match &existing_volume {
        Some(region) => {
            ui::status("Found", format!("volume {VOLUME} in {region}"));
            region.clone()
        }
        None => ask_region(fly)?,
    };

    let sizes = vm_sizes(fly);
    if !sizes.is_empty() {
        eprintln!("VM sizes (RAM is asked next):");
        for size in &sizes {
            let (_, max) = size.memory_range();
            eprintln!(
                "  {:<18} {:>2} CPUs, {:>6} RAM (up to {})",
                size.name,
                size.cpus,
                format_mb(size.memory_mb),
                format_mb(max)
            );
        }
    }
    let vm_size = loop {
        let name = ui::ask(
            "Machine size (more CPUs build faster, cost more per second)",
            Some(DEFAULT_VM_SIZE),
        )?;
        if sizes.is_empty() {
            break None;
        }
        if let Some(size) = sizes.iter().find(|s| s.name == name) {
            break Some(size);
        }
        ui::warn(format!("unknown size `{name}`"));
    };
    let (vm_size, memory_mb) = match vm_size {
        Some(size) => (size.name.clone(), ask_memory(size)?),
        None => (DEFAULT_VM_SIZE.to_owned(), None),
    };
    let memory_note = memory_mb.map_or(String::new(), |mb| format!(" with {} RAM", format_mb(mb)));

    let volume_gb = if existing_volume.is_some() {
        0
    } else {
        ask_number(
            "Volume size in GB (toolchain, registry cache, target dirs)",
            20,
        )?
    };
    let idle_minutes = ask_number("Stop the machine after how many idle minutes", 15)?;
    let ssh_key = ask_ssh_key()?;

    eprintln!();
    eprintln!(
        "  App:      {app}{}",
        match &org {
            Some(org) => format!(" (new, in org {org})"),
            None => " (existing)".into(),
        }
    );
    eprintln!("  Region:   {region}");
    eprintln!(
        "  Machine:  {MACHINE_NAME}, {vm_size}{memory_note}, stops after {idle_minutes} idle minutes"
    );
    match &existing_volume {
        Some(_) => eprintln!("  Volume:   {VOLUME} (existing)"),
        None => eprintln!("  Volume:   {VOLUME}, {volume_gb} GB"),
    }
    eprintln!("  SSH key:  {}", ssh_key.display());
    eprintln!();
    eprintln!("Costs (see https://fly.io/pricing):");
    eprintln!(
        "  - the machine, per second while it runs (it stops after {idle_minutes} idle minutes)"
    );
    eprintln!("  - the stopped machine's disk (the ~1 GB image), per GB per month");
    eprintln!(
        "  - the volume, per GB per month, until it's deleted; the machine stopping doesn't delete it"
    );
    eprintln!("Delete everything later with `cargo plane purge`.");

    Ok(Plan {
        org,
        region,
        vm_size,
        memory_mb,
        volume_gb,
        idle_minutes,
        ssh_key,
        volume_exists: existing_volume.is_some(),
    })
}

fn ask_org(fly: &Flyctl) -> Result<String> {
    // `orgs list --json` is a map of slug to name (or a list of objects).
    let orgs: Vec<String> = match fly.json::<Value, _>(&["orgs", "list", "--json"]) {
        Ok(Value::Object(map)) => map.keys().cloned().collect(),
        Ok(Value::Array(items)) => items
            .iter()
            .filter_map(|o| field(o, &["slug", "Slug"]))
            .collect(),
        _ => Vec::new(),
    };
    match orgs.as_slice() {
        [only] => Ok(only.clone()),
        _ => {
            if !orgs.is_empty() {
                eprintln!("Organizations: {}", orgs.join(", "));
            }
            ui::ask("Organization for the new app", Some("personal"))
        }
    }
}

fn ask_region(fly: &Flyctl) -> Result<String> {
    let regions = fly
        .json::<Value, _>(&["platform", "regions", "--json"])
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let regions: Vec<(String, String)> = regions
        .iter()
        .filter(|r| r.get("deprecated").and_then(Value::as_bool) != Some(true))
        .filter_map(|r| {
            let code = field(r, &["code", "Code"])?;
            Some((code, field(r, &["name", "Name"]).unwrap_or_default()))
        })
        .collect();
    if !regions.is_empty() {
        eprintln!("Regions:");
        for (code, name) in &regions {
            eprintln!("  {code}  {name}");
        }
    }
    loop {
        let region = ui::ask("Region (pick one close to you)", None)?;
        if regions.is_empty() || regions.iter().any(|(code, _)| *code == region) {
            return Ok(region);
        }
        ui::warn(format!("unknown region `{region}`"));
    }
}

fn ask_number(question: &str, default: u32) -> Result<u32> {
    loop {
        match ui::ask(question, Some(&default.to_string()))?.parse() {
            Ok(n) if n > 0 => return Ok(n),
            _ => ui::warn("enter a positive number"),
        }
    }
}

/// Default key names ssh tries without `-i`.
const DEFAULT_KEYS: &[&str] = &[
    "id_ed25519",
    "id_ecdsa",
    "id_rsa",
    "id_ed25519_sk",
    "id_ecdsa_sk",
];

fn ask_ssh_key() -> Result<PathBuf> {
    let ssh_dir = util::home_dir()?.join(".ssh");
    let mut keys: Vec<PathBuf> = fs::read_dir(&ssh_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.extension().is_some_and(|e| e == "pub") && p.with_extension("").is_file()
                })
                .collect()
        })
        .unwrap_or_default();
    // Default identities first, in ssh's order of preference.
    keys.sort_by_key(|p| {
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_owned();
        (
            DEFAULT_KEYS
                .iter()
                .position(|k| *k == stem)
                .unwrap_or(usize::MAX),
            stem,
        )
    });
    if keys.is_empty() {
        bail!(
            "no SSH key pair found in {}; create one with `ssh-keygen -t ed25519`",
            ssh_dir.display()
        );
    }
    eprintln!("SSH keys:");
    for (i, key) in keys.iter().enumerate() {
        eprintln!("  {}) {}", i + 1, key.display());
    }
    loop {
        let answer = ui::ask("SSH key to log in with", Some("1"))?;
        match answer.parse::<usize>() {
            Ok(n) if (1..=keys.len()).contains(&n) => return Ok(keys.swap_remove(n - 1)),
            _ => ui::warn(format!("pick a number from 1 to {}", keys.len())),
        }
    }
}

fn create(
    fly: &Flyctl,
    app: &str,
    plan: &Plan,
    old_machine: Option<&str>,
    opts: &Options,
) -> Result<()> {
    if let Some(org) = &plan.org {
        ui::status("Creating", format!("app {app}"));
        fly.interactive(&["apps", "create", app, "--org", org])?;
    }

    if !plan.volume_exists {
        ui::status(
            "Creating",
            format!("volume {VOLUME} ({} GB)", plan.volume_gb),
        );
        fly.interactive(&[
            "volumes",
            "create",
            VOLUME,
            "--app",
            app,
            "--region",
            &plan.region,
            "--size",
            &plan.volume_gb.to_string(),
            // It's a cache; snapshots would only add cost.
            "--scheduled-snapshots=false",
            "--yes",
        ])?;
    }

    if let Some(id) = old_machine {
        ui::status(
            "Destroying",
            format!("old builder machine {id} (the volume is kept)"),
        );
        fly.interactive(&["machine", "destroy", id, "--app", app, "--force"])?;
    }

    let key = fs::read_to_string(&plan.ssh_key)
        .with_context(|| format!("reading {}", plan.ssh_key.display()))?;
    let (context, dockerfile) = match &opts.dockerfile {
        Some(path) => {
            let path =
                fs::canonicalize(path).with_context(|| format!("finding {}", path.display()))?;
            (path.parent().unwrap().to_path_buf(), path)
        }
        None => {
            let dir = util::cache_dir()?.join("fly-build");
            write_build_files(&dir)?;
            (dir.clone(), dir.join("Dockerfile"))
        }
    };

    ui::status(
        "Building",
        "builder image on Fly.io's remote builder (takes a few minutes)",
    );
    let dockerfile = dockerfile
        .to_str()
        .context("Dockerfile path is not UTF-8")?;
    let mut args: Vec<String> = [
        "machine",
        "run",
        ".",
        "--dockerfile",
        dockerfile,
        "--app",
        app,
        "--region",
        &plan.region,
        "--vm-size",
        &plan.vm_size,
        "--volume",
        &format!("{VOLUME}:/data"),
        "--restart",
        "no",
        "--name",
        MACHINE_NAME,
        "--metadata",
        &format!("{METADATA_KEY}={METADATA_VALUE}"),
        "--env",
        &format!("SSH_AUTHORIZED_KEYS={}", key.trim()),
        "--env",
        &format!("IDLE_MINUTES={}", plan.idle_minutes),
    ]
    .map(String::from)
    .to_vec();
    if let Some(memory_mb) = plan.memory_mb {
        args.extend(["--vm-memory".into(), memory_mb.to_string()]);
    }
    let status = fly
        .command(&args)
        .current_dir(&context)
        .status()
        .context("running flyctl")?;
    if !status.success() {
        bail!("creating the builder machine failed ({status})");
    }
    Ok(())
}

/// The volume's region, if it exists.
fn find_volume(fly: &Flyctl, app: &str) -> Result<Option<String>> {
    let volumes: Value = fly.json(&["volumes", "list", "--app", app, "--json"])?;
    Ok(volumes.as_array().into_iter().flatten().find_map(|v| {
        (field(v, &["name", "Name"]).as_deref() == Some(VOLUME))
            .then(|| field(v, &["region", "Region"]))
            .flatten()
    }))
}

fn write_build_files(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join("Dockerfile"), DOCKERFILE)?;
    let entrypoint = dir.join("entrypoint.sh");
    fs::write(&entrypoint, ENTRYPOINT)?;
    fs::set_permissions(&entrypoint, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Add `[remotes.<name>]` to the global config, never overwriting.
fn write_config(
    path: &Path,
    name: &str,
    app: &str,
    configured_app: Option<&str>,
    ssh_key: Option<&Path>,
) -> Result<()> {
    let mut snippet = format!("\n[remotes.{name}]\nprovider = \"fly\"\napp = \"{app}\"\n");
    if let Some(key) = ssh_key {
        let private = key.with_extension("");
        let stem = private.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !DEFAULT_KEYS.contains(&stem) {
            snippet += &format!("ssh_options = [\"-i\", \"{}\"]\n", private.display());
        }
    }

    match configured_app {
        Some(existing) if existing == app => {
            ui::status("Configured", format!("remote `{name}` already uses {app}"));
        }
        Some(existing) => {
            ui::warn(format!(
                "remote `{name}` is configured for app `{existing}`; not touching it. \
                 To use the new app, put this in {}:\n{snippet}",
                path.display()
            ));
        }
        None => {
            fs::create_dir_all(path.parent().unwrap())?;
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            file.write_all(snippet.as_bytes())?;
            ui::status(
                "Configured",
                format!(
                    "remote `{name}` in {} (gitignore it if the machine is just yours)",
                    path.display()
                ),
            );
        }
    }
    Ok(())
}

fn smoke_test(name: &str) -> Result<()> {
    let config = Config::load(&std::env::current_dir()?)?;
    let remote = Remote::new(config.select(Some(name))?)?;
    let conn = remote.takeoff()?;
    let output = conn
        .shell("cargo --version")?
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        bail!(
            "connected, but `cargo --version` failed on the machine ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    ui::status(
        "Ready",
        format!(
            "{} on remote `{name}`; try `cargo plane --remote {name} check`",
            String::from_utf8_lossy(&output.stdout).trim()
        ),
    );
    Ok(())
}

/// The app this project's config already uses for `name`, if any.
fn configured_app(path: &Path, name: &str) -> Result<Option<String>> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(None);
    };
    let table: toml::Table =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(table
        .get("remotes")
        .and_then(|r| r.get(name))
        .and_then(|r| r.get("app"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned))
}

/// `cargo-plane-<project>-<user>`: recognizable in the Fly dashboard and
/// likely unique. Fly app names are lowercase letters, digits and dashes.
fn default_app_name(project: &str, user: &str) -> String {
    let slug = |s: &str| {
        s.to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("-")
    };
    let mut name = format!("cargo-plane-{}", slug(project));
    let user = slug(user);
    if !user.is_empty() {
        name = format!("{name}-{user}");
    }
    name.truncate(63);
    name.trim_end_matches('-').to_owned()
}

/// First of `keys` present on a JSON object, as a string.
fn field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| value.get(*k))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

struct VmSize {
    name: String,
    cpus: u64,
    /// The preset's default memory.
    memory_mb: u64,
}

impl VmSize {
    /// Allowed memory: 256 MB-2 GB per shared CPU, 2-8 GB per performance CPU
    /// (https://fly.io/docs/machines/guides-examples/machine-sizing/).
    fn memory_range(&self) -> (u64, u64) {
        let (min, max) = if self.name.starts_with("shared") {
            (256, 2048)
        } else {
            (2048, 8192)
        };
        (min * self.cpus, max * self.cpus)
    }
}

/// Sizes from `platform vm-sizes --json` (a map keyed by name), shared CPUs
/// first, then by size. Empty if flyctl's output changes shape.
fn vm_sizes(fly: &Flyctl) -> Vec<VmSize> {
    let Ok(Value::Object(map)) = fly.json::<Value, _>(&["platform", "vm-sizes", "--json"]) else {
        return Vec::new();
    };
    let mut sizes: Vec<_> = map
        .iter()
        .map(|(name, spec)| {
            let num = |key| spec.get(key).and_then(Value::as_u64).unwrap_or(0);
            VmSize {
                name: name.clone(),
                cpus: num("cpus"),
                memory_mb: num("memory_mb"),
            }
        })
        .filter(|s| s.cpus > 0)
        .collect();
    sizes.sort_by_key(|s| (!s.name.starts_with("shared"), s.cpus, s.memory_mb));
    sizes
}

/// Ask for RAM within the size's limits; `None` keeps the preset.
fn ask_memory(size: &VmSize) -> Result<Option<u64>> {
    let (min, max) = size.memory_range();
    let question = format!(
        "RAM for {} ({} to {}; Rust builds like lots of it)",
        size.name,
        format_mb(min),
        format_mb(max)
    );
    loop {
        let answer = ui::ask(&question, Some(&format_mb(size.memory_mb)))?;
        match parse_memory(&answer) {
            Some(mb) if mb == size.memory_mb => return Ok(None),
            Some(mb) if (min..=max).contains(&mb) && mb % 256 == 0 => return Ok(Some(mb)),
            Some(_) => ui::warn(format!(
                "pick a multiple of 256 MB from {} to {}",
                format_mb(min),
                format_mb(max)
            )),
            None => ui::warn("enter a size like 16, 16G or 512M"),
        }
    }
}

/// `16`, `16G`, `16 GB` (gigabytes) or `512M`, `512 MB` (megabytes), in MB.
fn parse_memory(input: &str) -> Option<u64> {
    let lower = input.trim().to_ascii_lowercase();
    let (number, unit) = match lower.find(|c: char| !(c.is_ascii_digit() || c == '.')) {
        Some(i) => (&lower[..i], lower[i..].trim()),
        None => (lower.as_str(), ""),
    };
    let number: f64 = number.parse().ok()?;
    let mb = match unit {
        "" | "g" | "gb" | "gib" => number * 1024.0,
        "m" | "mb" | "mib" => number,
        _ => return None,
    };
    (mb > 0.0 && mb.fract() == 0.0).then_some(mb as u64)
}

fn format_mb(mb: u64) -> String {
    match mb {
        mb if mb % 1024 == 0 => format!("{} GB", mb / 1024),
        mb if mb > 1024 => format!("{:.1} GB", mb as f64 / 1024.0),
        mb => format!("{mb} MB"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_name_from_project() {
        // SAFETY: tests in this module don't read USER concurrently.
        unsafe { std::env::set_var("USER", "Robin") };
        assert_eq!(
            default_app_name("tileception", "Robin"),
            "cargo-plane-tileception-robin"
        );
        assert_eq!(
            default_app_name("My_Crate.rs", "robin"),
            "cargo-plane-my-crate-rs-robin"
        );
        assert!(default_app_name(&"x".repeat(100), "robin").len() <= 63);
    }

    #[test]
    fn parses_memory() {
        assert_eq!(parse_memory("16"), Some(16384));
        assert_eq!(parse_memory("16G"), Some(16384));
        assert_eq!(parse_memory(" 1.5 gb "), Some(1536));
        assert_eq!(parse_memory("512M"), Some(512));
        assert_eq!(parse_memory("lots"), None);
        assert_eq!(parse_memory("0"), None);
    }

    #[test]
    fn memory_limits_scale_with_cpus() {
        let size = |name: &str, cpus| VmSize {
            name: name.into(),
            cpus,
            memory_mb: 0,
        };
        assert_eq!(size("performance-4x", 4).memory_range(), (8192, 32768));
        assert_eq!(size("shared-cpu-1x", 1).memory_range(), (256, 2048));
    }

    #[test]
    fn formats_memory() {
        assert_eq!(format_mb(256), "256 MB");
        assert_eq!(format_mb(1536), "1.5 GB");
        assert_eq!(format_mb(16384), "16 GB");
    }
}
