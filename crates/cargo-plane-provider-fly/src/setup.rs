//! `cargo plane setup fly`: install flyctl, log in, create the app, volume and
//! builder machine, and add the remote to the project's config.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use cargo_plane_provider::{Host, Setup};
use serde_json::{Value, json};

use super::flyctl::{self, Flyctl, METADATA_KEY, METADATA_VALUE};

const DOCKERFILE: &str = include_str!("../image/Dockerfile");
const ENTRYPOINT: &str = include_str!("../image/entrypoint.sh");
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
  -y, --yes            accept defaults, installation and resource creation without prompts
  --app <name>         app name (default: configured app, or cargo-plane-<project>-<user>)
  --org <slug>         organization (default: only available org, otherwise personal)
  --region <code>      region; required with --yes
  --machine <size>     machine size (default: performance-4x)
  --ram <size>         RAM, e.g. 16, 16G or 512M (default: machine preset)
  --volume-size <GB>   volume size in GB (default: 20)
  --idle-timeout <min> stop after this many idle minutes (default: 15)
  --ssh-key <path>     public or private SSH key path (default: first detected key pair)
  --rebuild             recreate the machine (new image or settings); the volume is kept
  --dockerfile <path>   build from your own Dockerfile (see --eject)
  --eject <dir>         write the default Dockerfile and entrypoint to <dir> and exit

--yes requires authentication beforehand and never starts browser login.
Existing builders are reused unless --rebuild is passed. Existing volumes
are kept; their region must match --region and --volume-size does not resize them.
";

#[derive(Default)]
struct Options {
    yes: bool,
    app: Option<String>,
    org: Option<String>,
    region: Option<String>,
    machine: Option<String>,
    memory_mb: Option<u64>,
    volume_gb: Option<u32>,
    idle_minutes: Option<u32>,
    ssh_key: Option<PathBuf>,
    rebuild: bool,
    dockerfile: Option<PathBuf>,
    eject: Option<PathBuf>,
}

pub fn setup(setup: &Setup, host: &dyn Host) -> Result<()> {
    let Some(opts) = parse_args(&setup.args)? else {
        print!("{USAGE}");
        return Ok(());
    };
    if let Some(dir) = &opts.eject {
        write_build_files(dir)?;
        host.status(
            "Wrote",
            &format!("Dockerfile and entrypoint.sh to {}", dir.display()),
        );
        return Ok(());
    }
    if opts.yes && opts.region.is_none() {
        bail!("setup fly --yes requires --region <code>");
    }
    if !opts.yes && !host.interactive() {
        bail!(
            "`cargo plane setup fly` asks questions; pass --yes with --region, or run it in a terminal"
        );
    }
    let remote_name = &setup.remote;

    let fly = ensure_flyctl(host, opts.yes)?;
    ensure_login(&fly, host, opts.yes)?;

    // One throwaway machine per project, configured in that project.
    let root = setup
        .project_dir
        .as_ref()
        .context("run `cargo plane setup fly` inside the project you want a build machine for")?;
    let configured_app = setup
        .config
        .as_ref()
        .and_then(|c| c.get("app"))
        .and_then(Value::as_str);
    let project = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let default_app = configured_app
        .map(str::to_owned)
        .unwrap_or_else(|| default_app_name(project, &std::env::var("USER").unwrap_or_default()));
    let app = match &opts.app {
        Some(app) => app.clone(),
        None => ask(
            host,
            "App name (globally unique on Fly.io)",
            Some(&default_app),
            opts.yes,
        )?,
    };

    let app_exists = fly.app_exists(&app);
    let builder = if app_exists {
        fly.builder(&app, None)?
    } else {
        None
    };

    match &builder {
        Some(machine) if !opts.rebuild => {
            host.status(
                "Found",
                &format!(
                    "builder machine {} in {app}; use --rebuild to recreate it",
                    machine.id
                ),
            );
            save_config(host, remote_name, &app, configured_app, None)?;
        }
        _ => {
            let plan = ask_plan(&fly, host, &app, app_exists, &opts)?;
            if !opts.yes && !host.confirm("Create these resources?", false)? {
                bail!("aborted");
            }
            // Before creating anything, so `purge` can clean up a failed setup.
            save_config(host, remote_name, &app, configured_app, Some(&plan.ssh_key))?;
            create(
                &fly,
                host,
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
    Ok(())
}

fn parse_args(args: &[String]) -> Result<Option<Options>> {
    let mut opts = Options::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-y" | "--yes" => opts.yes = true,
            "--app" | "--org" | "--region" | "--machine" | "--ram" | "--volume-size"
            | "--idle-timeout" | "--ssh-key" => {
                let value = iter
                    .next()
                    .with_context(|| format!("{arg} needs a value"))?;
                if value.trim().is_empty() || value.starts_with('-') {
                    bail!("{arg} needs a nonempty value");
                }
                match arg.as_str() {
                    "--app" => opts.app = Some(value.clone()),
                    "--org" => opts.org = Some(value.clone()),
                    "--region" => opts.region = Some(value.clone()),
                    "--machine" => opts.machine = Some(value.clone()),
                    "--ssh-key" => opts.ssh_key = Some(value.into()),
                    "--ram" => {
                        opts.memory_mb = Some(
                            parse_memory(value)
                                .context("--ram needs a size like 16, 16G or 512M")?,
                        )
                    }
                    "--volume-size" | "--idle-timeout" => {
                        let n = value
                            .parse::<u32>()
                            .ok()
                            .filter(|n| *n > 0)
                            .with_context(|| format!("{arg} must be a positive whole number"))?;
                        if arg == "--volume-size" {
                            opts.volume_gb = Some(n);
                        } else {
                            opts.idle_minutes = Some(n);
                        }
                    }
                    _ => unreachable!(),
                }
            }
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

fn ask(host: &dyn Host, question: &str, default: Option<&str>, yes: bool) -> Result<String> {
    if yes {
        return default
            .map(str::to_owned)
            .with_context(|| format!("no default for {question}"));
    }
    host.ask(question, default)
}

fn ensure_flyctl(host: &dyn Host, yes: bool) -> Result<Flyctl> {
    if let Some(fly) = Flyctl::locate(None) {
        host.status("Found", &format!("flyctl at {}", fly.path().display()));
        return Ok(fly);
    }
    eprintln!("cargo-plane manages Fly.io machines through flyctl, which isn't installed.");
    let question = format!(
        "Download the latest flyctl from GitHub into {}?",
        flyctl::install_dir()?.display()
    );
    if !yes && !host.confirm(&question, true)? {
        bail!(
            "flyctl is required; install it yourself (https://fly.io/docs/flyctl/install/) and rerun"
        );
    }
    flyctl::install(host)
}

fn ensure_login(fly: &Flyctl, host: &dyn Host, yes: bool) -> Result<()> {
    if !fly.logged_in() {
        if yes {
            bail!(
                "Fly.io authentication is required for --yes; run `flyctl auth login` beforehand or provide FLY_API_TOKEN"
            );
        }
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
    host.status("Logged in", &format!("as {}", who.trim()));
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

fn ask_plan(
    fly: &Flyctl,
    host: &dyn Host,
    app: &str,
    app_exists: bool,
    opts: &Options,
) -> Result<Plan> {
    let org = if app_exists {
        None
    } else {
        Some(match &opts.org {
            Some(org) => org.clone(),
            None => ask_org(fly, host, opts.yes)?,
        })
    };

    let existing_volume = if app_exists {
        find_volume(fly, app)?
    } else {
        None
    };
    let region = match &existing_volume {
        Some(region) => {
            if opts
                .region
                .as_ref()
                .is_some_and(|requested| requested != region)
            {
                bail!(
                    "existing volume is in {region}, which conflicts with --region {}; reuse that region or choose another app",
                    opts.region.as_ref().unwrap()
                );
            }
            host.status("Found", &format!("volume {VOLUME} in {region}"));
            region.clone()
        }
        None => ask_region(fly, host, opts.region.as_deref())?,
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
        let name = match &opts.machine {
            Some(name) => name.clone(),
            None => ask(
                host,
                "Machine size (more CPUs build faster, cost more per second)",
                Some(DEFAULT_VM_SIZE),
                opts.yes,
            )?,
        };
        if sizes.is_empty() {
            break None;
        }
        if let Some(size) = sizes.iter().find(|s| s.name == name) {
            break Some(size);
        }
        if opts.machine.is_some() || opts.yes {
            bail!("unknown machine size `{name}`");
        }
        host.warn(&format!("unknown size `{name}`"));
    };
    let (vm_size, memory_mb) = match vm_size {
        Some(size) => (size.name.clone(), ask_memory(host, size, opts)?),
        None => {
            if opts.memory_mb.is_some() {
                bail!(
                    "can't validate --ram because Fly.io machine sizes are unavailable; retry when sizes can be fetched"
                );
            }
            (
                opts.machine
                    .clone()
                    .unwrap_or_else(|| DEFAULT_VM_SIZE.to_owned()),
                None,
            )
        }
    };
    let memory_note = memory_mb.map_or(String::new(), |mb| format!(" with {} RAM", format_mb(mb)));

    let volume_gb = if existing_volume.is_some() {
        0
    } else {
        match opts.volume_gb {
            Some(gb) => gb,
            None => ask_number(
                host,
                "Volume size in GB (toolchain, registry cache, target dirs)",
                20,
                opts.yes,
            )?,
        }
    };
    let idle_minutes = match opts.idle_minutes {
        Some(minutes) => minutes,
        None => ask_number(
            host,
            "Stop the machine after how many idle minutes",
            15,
            opts.yes,
        )?,
    };
    let ssh_key = match &opts.ssh_key {
        Some(path) => {
            let public = if path.extension().is_some_and(|ext| ext == "pub") {
                path.clone()
            } else {
                PathBuf::from(format!("{}.pub", path.display()))
            };
            if !public.is_file() || !public.with_extension("").is_file() {
                bail!(
                    "--ssh-key needs an existing SSH key pair: {}",
                    path.display()
                );
            }
            public
        }
        None => ask_ssh_key(host, opts.yes)?,
    };

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

fn ask_org(fly: &Flyctl, host: &dyn Host, yes: bool) -> Result<String> {
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
            ask(host, "Organization for the new app", Some("personal"), yes)
        }
    }
}

fn ask_region(fly: &Flyctl, host: &dyn Host, requested: Option<&str>) -> Result<String> {
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
        let region = match requested {
            Some(region) => region.to_owned(),
            None => host.ask("Region (pick one close to you)", None)?,
        };
        if regions.is_empty() || regions.iter().any(|(code, _)| *code == region) {
            return Ok(region);
        }
        if requested.is_some() {
            bail!("unknown region `{region}`");
        }
        host.warn(&format!("unknown region `{region}`"));
    }
}

fn ask_number(host: &dyn Host, question: &str, default: u32, yes: bool) -> Result<u32> {
    loop {
        match ask(host, question, Some(&default.to_string()), yes)?.parse() {
            Ok(n) if n > 0 => return Ok(n),
            _ => host.warn("enter a positive number"),
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

fn ask_ssh_key(host: &dyn Host, yes: bool) -> Result<PathBuf> {
    let ssh_dir = cargo_plane_provider::home_dir()?.join(".ssh");
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
        let answer = ask(host, "SSH key to log in with", Some("1"), yes)?;
        match answer.parse::<usize>() {
            Ok(n) if (1..=keys.len()).contains(&n) => return Ok(keys.swap_remove(n - 1)),
            _ => host.warn(&format!("pick a number from 1 to {}", keys.len())),
        }
    }
}

fn create(
    fly: &Flyctl,
    host: &dyn Host,
    app: &str,
    plan: &Plan,
    old_machine: Option<&str>,
    opts: &Options,
) -> Result<()> {
    if let Some(org) = &plan.org {
        host.status("Creating", &format!("app {app}"));
        run_fly(fly, &["apps", "create", app, "--org", org], opts.yes)?;
    }

    if !plan.volume_exists {
        host.status(
            "Creating",
            &format!("volume {VOLUME} ({} GB)", plan.volume_gb),
        );
        run_fly(
            fly,
            &[
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
            ],
            opts.yes,
        )?;
    }

    if let Some(id) = old_machine {
        host.status(
            "Destroying",
            &format!("old builder machine {id} (the volume is kept)"),
        );
        run_fly(
            fly,
            &["machine", "destroy", id, "--app", app, "--force"],
            opts.yes,
        )?;
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
            let dir = cargo_plane_provider::cache_dir()?.join("fly-build");
            write_build_files(&dir)?;
            (dir.clone(), dir.join("Dockerfile"))
        }
    };

    host.status(
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
    let mut command = fly.command(&args);
    command.current_dir(&context);
    if opts.yes {
        command.stdin(Stdio::null());
    }
    let status = command.status().context("running flyctl")?;
    if !status.success() {
        bail!("creating the builder machine failed ({status})");
    }
    Ok(())
}

fn run_fly(fly: &Flyctl, args: &[&str], yes: bool) -> Result<()> {
    if !yes {
        return fly.interactive(args);
    }
    let status = fly
        .command(args)
        .stdin(Stdio::null())
        .status()
        .context("running flyctl")?;
    if !status.success() {
        bail!("`flyctl {}` failed ({status})", args.join(" "));
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

/// Point the remote at `app` and record the selected SSH identity.
fn save_config(
    host: &dyn Host,
    name: &str,
    app: &str,
    configured_app: Option<&str>,
    ssh_key: Option<&Path>,
) -> Result<()> {
    let mut config = json!({ "app": app });
    if let Some(key) = ssh_key {
        let private = key.with_extension("");
        config["ssh_options"] = json!(["-i", private.display().to_string()]);
    }
    if let Some(old) = configured_app.filter(|old| *old != app) {
        host.warn(&format!(
            "remote `{name}` used app `{old}` and now uses `{app}`; `{old}` still exists \
             (delete it with `flyctl apps destroy {old}`)"
        ));
    }
    host.save_config(&config)
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
fn ask_memory(host: &dyn Host, size: &VmSize, opts: &Options) -> Result<Option<u64>> {
    let (min, max) = size.memory_range();
    if let Some(mb) = opts.memory_mb {
        if !(min..=max).contains(&mb) || mb % 256 != 0 {
            bail!(
                "--ram for {} must be a multiple of 256 MB from {} to {}",
                size.name,
                format_mb(min),
                format_mb(max)
            );
        }
        return Ok((mb != size.memory_mb).then_some(mb));
    }
    if opts.yes {
        return Ok(None);
    }
    let question = format!(
        "RAM for {} ({} to {}; Rust builds like lots of it)",
        size.name,
        format_mb(min),
        format_mb(max)
    );
    loop {
        let answer = host.ask(&question, Some(&format_mb(size.memory_mb)))?;
        match parse_memory(&answer) {
            Some(mb) if mb == size.memory_mb => return Ok(None),
            Some(mb) if (min..=max).contains(&mb) && mb % 256 == 0 => return Ok(Some(mb)),
            Some(_) => host.warn(&format!(
                "pick a multiple of 256 MB from {} to {}",
                format_mb(min),
                format_mb(max)
            )),
            None => host.warn("enter a size like 16, 16G or 512M"),
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
