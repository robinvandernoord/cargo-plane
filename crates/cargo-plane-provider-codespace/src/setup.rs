//! `cargo plane setup codespace`: log in to GitHub, create (or pick) a
//! codespace for the project's repository, add it to the project's config and
//! make sure it has what cargo-plane needs.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use cargo_plane_provider::{Host, Setup};
use serde::Deserialize;
use serde_json::{Value, json};

use super::gh::Gh;

const USAGE: &str = "\
Usage: cargo plane [--remote <name>] setup codespace [options]

Interactively logs in to GitHub (with the `codespace` scope) if needed, creates
a codespace for the current project's repository or picks an existing one, and
adds the remote (default name: codespace) to the project's .cargo/plane.toml.
Then checks that the codespace has rsync, a C compiler and cargo, and offers
to install what's missing. Safe to rerun. Delete the codespace with
`cargo plane purge codespace`.

Options:
  -y, --yes                  accept defaults, creation and tool installation without prompts
  --repo <owner/repo>         repository (default: detected from the current project)
  --codespace <name>          use an existing codespace instead of creating or picking one
  --machine <name>            machine type (default: a 4-CPU machine, or largest available)
  --ram <GB>                  choose the fewest CPUs with at least this much RAM (whole GB)
  --idle-timeout <minutes>    stop after 5-240 idle minutes (default: 30)
  --devcontainer-path <path>  devcontainer.json to use (default: GitHub's default)

--machine and --ram are mutually exclusive. Creation options only affect new
codespaces; an existing configured codespace is reused. --yes creates a new
codespace when none is configured, and requires GitHub authentication with
codespace access to be ready beforehand. It never starts a login prompt.
";

/// GitHub's own default; the maximum is 240.
const DEFAULT_IDLE_MINUTES: u32 = 30;

#[derive(Default)]
struct Options {
    yes: bool,
    repo: Option<String>,
    codespace: Option<String>,
    machine: Option<String>,
    ram: Option<u64>,
    idle_minutes: Option<u32>,
    devcontainer_path: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Option<Options>> {
    let mut opts = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-y" | "--yes" => opts.yes = true,
            "--repo"
            | "--codespace"
            | "--machine"
            | "--ram"
            | "--idle-timeout"
            | "--devcontainer-path" => {
                let value = args
                    .next()
                    .with_context(|| format!("{arg} needs a value"))?;
                if value.trim().is_empty() || value.starts_with('-') {
                    bail!("{arg} needs a nonempty value");
                }
                match arg.as_str() {
                    "--repo" => opts.repo = Some(value.clone()),
                    "--codespace" => opts.codespace = Some(value.clone()),
                    "--machine" => opts.machine = Some(value.clone()),
                    "--devcontainer-path" => opts.devcontainer_path = Some(value.clone()),
                    "--ram" => {
                        let gb = value
                            .parse::<u64>()
                            .ok()
                            .filter(|n| *n > 0)
                            .context("--ram must be a positive whole number of GB")?;
                        opts.ram = Some(gb.checked_mul(1 << 30).context("--ram is too large")?);
                    }
                    "--idle-timeout" => {
                        opts.idle_minutes = Some(
                            value
                                .parse::<u32>()
                                .ok()
                                .filter(|n| (5..=240).contains(n))
                                .context("--idle-timeout must be a number from 5 to 240")?,
                        );
                    }
                    _ => unreachable!(),
                }
            }
            other => bail!("unknown option `{other}` for `setup codespace`\n\n{USAGE}"),
        }
    }
    if opts.machine.is_some() && opts.ram.is_some() {
        bail!("--machine and --ram are mutually exclusive");
    }
    Ok(Some(opts))
}

pub fn setup(setup: &Setup, host: &dyn Host) -> Result<()> {
    let Some(opts) = parse_args(&setup.args)? else {
        print!("{USAGE}");
        return Ok(());
    };
    if !opts.yes && !host.interactive() {
        bail!("`cargo plane setup codespace` asks questions; pass --yes or run it in a terminal");
    }
    let remote = &setup.remote;

    let gh = Gh::locate(None).context(
        "cargo-plane manages codespaces through the GitHub CLI; install `gh` \
         (https://cli.github.com) and rerun",
    )?;
    host.status("Found", &format!("gh at {}", gh.path().display()));
    ensure_login(&gh, opts.yes)?;

    let root = setup
        .project_dir
        .as_ref()
        .context("run `cargo plane setup codespace` inside the project you want a codespace for")?;
    let configured = setup
        .config
        .as_ref()
        .and_then(|c| c.get("codespace"))
        .and_then(Value::as_str);

    let name = if let Some(name) = &opts.codespace {
        gh.codespace(name)?;
        host.warn(
            "`cargo plane purge` deletes the whole codespace, including your work in it; \
             only use it on one you can throw away",
        );
        name.clone()
    } else {
        match configured {
            Some(name) if gh.codespace(name).is_ok() => {
                host.status("Found", &format!("codespace {name} for remote `{remote}`"));
                name.to_owned()
            }
            configured => {
                if let Some(name) = configured {
                    host.warn(&format!(
                        "codespace `{name}` for remote `{remote}` doesn't exist anymore"
                    ));
                }
                let project = root
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("project");
                pick_or_create(&gh, host, root, project, &opts)?
            }
        }
    };
    host.save_config(&json!({ "codespace": name }))?;

    provision(host, opts.yes)
}

fn ensure_login(gh: &Gh, yes: bool) -> Result<()> {
    if yes {
        if !gh.has_codespace_scope() {
            bail!(
                "GitHub authentication with codespace access is required for --yes; \
                 run `gh auth login --scopes codespace` or `gh auth refresh --scopes codespace` \
                 beforehand, or provide GH_TOKEN with codespace access"
            );
        }
        return Ok(());
    }
    if !gh.logged_in() {
        eprintln!(
            "\nYou're not logged in to GitHub. gh will now log you in, with access to\n\
             your codespaces. Come back here when it's done.\n"
        );
        gh.interactive(&[
            "auth",
            "login",
            "--hostname",
            "github.com",
            "--scopes",
            "codespace",
        ])?;
    } else if !gh.has_codespace_scope() {
        eprintln!(
            "\ngh needs access to your codespaces (the `codespace` scope). It will now\n\
             ask GitHub for it. Come back here when it's done.\n"
        );
        gh.interactive(&[
            "auth",
            "refresh",
            "--hostname",
            "github.com",
            "--scopes",
            "codespace",
        ])?;
    }
    if !gh.has_codespace_scope() {
        bail!("gh still can't access your codespaces; check `gh auth status`");
    }
    Ok(())
}

/// The name of an existing codespace the user picks, or of a new one.
fn pick_or_create(
    gh: &Gh,
    host: &dyn Host,
    root: &Path,
    project: &str,
    opts: &Options,
) -> Result<String> {
    let default_repo = opts.repo.clone().or_else(|| {
        gh.command(&[
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "--jq",
            ".nameWithOwner",
        ])
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|r| !r.is_empty())
    });
    let repo = if opts.repo.is_some() || opts.yes {
        default_repo.context("can't detect the GitHub repository; pass --repo <owner/repo>")?
    } else {
        host.ask(
            "Repository to create the codespace from (owner/repo)",
            default_repo.as_deref(),
        )?
    };

    if opts.yes {
        return create(gh, host, &repo, project, opts);
    }

    let existing = gh.codespaces(&repo)?;
    if !existing.is_empty() {
        eprintln!("Codespaces for {repo}:");
        for (i, cs) in existing.iter().enumerate() {
            eprintln!(
                "  {}) {} ({}, {}, {})",
                i + 1,
                cs.name,
                cs.display_name,
                cs.machine_name,
                cs.state
            );
        }
        eprintln!("  n) create a new one");
        loop {
            let answer = host.ask("Codespace to build on", Some("n"))?;
            if answer == "n" {
                break;
            }
            match answer.parse::<usize>() {
                Ok(n) if (1..=existing.len()).contains(&n) => {
                    host.warn(
                        "`cargo plane purge` deletes the whole codespace, including your \
                         work in it; only use it on one you can throw away",
                    );
                    return Ok(existing[n - 1].name.clone());
                }
                _ => host.warn(&format!("pick a number from 1 to {}, or n", existing.len())),
            }
        }
    }

    create(gh, host, &repo, project, opts)
}

#[derive(Deserialize)]
struct Machines {
    machines: Vec<Machine>,
}

#[derive(Deserialize)]
struct Machine {
    name: String,
    #[serde(default)]
    cpus: u64,
    #[serde(default)]
    memory_in_bytes: u64,
    #[serde(default)]
    storage_in_bytes: u64,
}

fn create(gh: &Gh, host: &dyn Host, repo: &str, project: &str, opts: &Options) -> Result<String> {
    let mut machines = gh
        .json::<Machines, _>(&["api", &format!("repos/{repo}/codespaces/machines")])
        .map(|m| m.machines)
        .unwrap_or_default();
    machines.sort_by_key(|m| (m.cpus, m.memory_in_bytes));
    let gb = |bytes: u64| bytes / (1 << 30);
    if !machines.is_empty() {
        eprintln!("Machine types:");
        for m in &machines {
            eprintln!(
                "  {:<22} {:>2} CPUs, {:>3} GB RAM, {:>3} GB disk",
                m.name,
                m.cpus,
                gb(m.memory_in_bytes),
                gb(m.storage_in_bytes)
            );
        }
    }
    let default_machine = machines
        .iter()
        .find(|m| m.cpus == 4)
        .or(machines.last())
        .map(|m| m.name.as_str());
    let machine = if let Some(name) = &opts.machine {
        if !machines.is_empty() && !machines.iter().any(|m| m.name == *name) {
            bail!("unknown machine type `{name}`");
        }
        name.clone()
    } else if let Some(ram) = opts.ram {
        machines
            .iter()
            .find(|m| m.memory_in_bytes >= ram)
            .with_context(|| format!("no available machine has at least {} GB RAM", gb(ram)))?
            .name
            .clone()
    } else if opts.yes {
        default_machine
            .context("can't determine a default machine; pass --machine <name>")?
            .to_owned()
    } else {
        loop {
            let name = host.ask(
                "Machine type (more CPUs build faster, use up your quota faster)",
                default_machine,
            )?;
            if machines.is_empty() || machines.iter().any(|m| m.name == name) {
                break name;
            }
            host.warn(&format!("unknown machine type `{name}`"));
        }
    };
    let idle_minutes = if let Some(minutes) = opts.idle_minutes {
        minutes
    } else if opts.yes {
        DEFAULT_IDLE_MINUTES
    } else {
        loop {
            match host
                .ask(
                    "Stop the codespace after how many idle minutes (5-240)",
                    Some(&DEFAULT_IDLE_MINUTES.to_string()),
                )?
                .parse::<u32>()
            {
                Ok(n) if (5..=240).contains(&n) => break n,
                _ => host.warn("enter a number from 5 to 240"),
            }
        }
    };
    let display_name: String = format!("cargo-plane {project}").chars().take(48).collect();

    eprintln!();
    eprintln!("  Repository:  {repo} (default branch)");
    eprintln!("  Codespace:   {display_name}, {machine}");
    eprintln!("  Idle stop:   after {idle_minutes} minutes");
    eprintln!();
    eprintln!(
        "Costs (see https://docs.github.com/billing/managing-billing-for-github-codespaces):"
    );
    eprintln!("  - compute, per core-hour while the codespace runs");
    eprintln!("  - storage, per GB-month until the codespace is deleted");
    eprintln!("  Personal accounts get a monthly allowance of both for free. GitHub deletes");
    eprintln!("  stopped codespaces after their retention period (30 days by default).");
    eprintln!("Delete it earlier with `cargo plane purge`.");
    if !opts.yes && !host.confirm("Create this codespace?", false)? {
        bail!("aborted");
    }

    if opts.yes {
        // The API uses GitHub's default devcontainer without gh's hidden
        // devcontainer and additional-permission prompts. It returns the name
        // directly, even if another codespace is created concurrently.
        host.status(
            "Creating",
            &format!("codespace for {repo} (takes a minute or two)"),
        );
        let mut args = vec![
            "api".to_owned(),
            format!("repos/{repo}/codespaces"),
            "--method".into(),
            "POST".into(),
            "-f".into(),
            format!("machine={machine}"),
            "-f".into(),
            format!("display_name={display_name}"),
            "-F".into(),
            format!("idle_timeout_minutes={idle_minutes}"),
            "-F".into(),
            "multi_repo_permissions_opt_out=true".into(),
        ];
        if let Some(path) = &opts.devcontainer_path {
            args.extend(["-f".into(), format!("devcontainer_path={path}")]);
        }
        let created: Value = gh.json(&args)?;
        return created
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("GitHub returned no name for the created codespace");
    }

    // gh may ask questions (which dev container to use), so let it talk to
    // the terminal and find the new codespace afterwards.
    let before: BTreeSet<String> = gh.codespaces(repo)?.into_iter().map(|c| c.name).collect();
    host.status(
        "Creating",
        &format!("codespace for {repo} (takes a minute or two)"),
    );
    let mut args = vec![
        "codespace",
        "create",
        "--repo",
        repo,
        "--machine",
        &machine,
        "--idle-timeout",
        &format!("{idle_minutes}m"),
        "--display-name",
        &display_name,
        "--status",
    ]
    .into_iter()
    .map(String::from)
    .collect::<Vec<_>>();
    if let Some(path) = &opts.devcontainer_path {
        args.extend(["--devcontainer-path".into(), path.clone()]);
    }
    gh.interactive(&args)?;
    let new: Vec<String> = gh
        .codespaces(repo)?
        .into_iter()
        .map(|c| c.name)
        .filter(|name| !before.contains(name))
        .collect();
    match new.as_slice() {
        [name] => Ok(name.clone()),
        _ => bail!(
            "can't tell which codespace was just created; add it to .cargo/plane.toml yourself \
             (`provider = \"codespace\"`, `codespace = \"<name from gh codespace list>\"`)"
        ),
    }
}

/// Install what's missing for cargo-plane, after asking.
fn provision(host: &dyn Host, yes: bool) -> Result<()> {
    let missing = host.run_remote(
        "for t in rsync cc cargo; do command -v $t >/dev/null 2>&1 || echo $t; done",
        true,
    )?;
    if !missing.success {
        bail!(
            "checking the codespace's tools failed: {}",
            missing.stderr.trim()
        );
    }
    let missing: Vec<&str> = missing.stdout.split_whitespace().collect();
    if missing.iter().any(|t| *t == "rsync" || *t == "cc") {
        let question = "The codespace lacks rsync or a C compiler. Install rsync and \
                        build-essential with apt-get?";
        if !yes && !host.confirm(question, true)? {
            bail!("cargo-plane needs rsync and a C compiler on the codespace");
        }
        run(
            host,
            "sudo apt-get update && \
             sudo DEBIAN_FRONTEND=noninteractive apt-get install -y rsync build-essential",
        )?;
    }
    if missing.contains(&"cargo") {
        let question = "The codespace has no Rust toolchain. Install one with rustup into \
                        its home directory? (A rebuild of the codespace removes it; add the \
                        `rust` dev container feature to keep it.)";
        if !yes && !host.confirm(question, true)? {
            bail!("cargo-plane needs cargo on the codespace");
        }
        run(
            host,
            "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
             sh -s -- -y --profile minimal --component clippy",
        )?;
    }
    Ok(())
}

fn run(host: &dyn Host, command: &str) -> Result<()> {
    let output = host.run_remote(command, false)?;
    if !output.success {
        bail!(
            "`{command}` failed on the codespace (exit code {:?})",
            output.code
        );
    }
    Ok(())
}
