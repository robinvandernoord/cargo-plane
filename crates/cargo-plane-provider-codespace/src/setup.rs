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
Usage: cargo plane [--remote <name>] setup codespace

Interactively logs in to GitHub (with the `codespace` scope) if needed, creates
a codespace for the current project's repository or picks an existing one, and
adds the remote (default name: codespace) to the project's .cargo/plane.toml.
Then checks that the codespace has rsync, a C compiler and cargo, and offers
to install what's missing. Safe to rerun. Delete the codespace with
`cargo plane purge codespace`.
";

/// GitHub's own default; the maximum is 240.
const DEFAULT_IDLE_MINUTES: u32 = 30;

pub fn setup(setup: &Setup, host: &dyn Host) -> Result<()> {
    match setup.args.as_slice() {
        [] => {}
        [help] if help == "-h" || help == "--help" => {
            print!("{USAGE}");
            return Ok(());
        }
        [other, ..] => bail!("unknown option `{other}` for `setup codespace`\n\n{USAGE}"),
    }
    if !host.interactive() {
        bail!("`cargo plane setup codespace` asks questions; run it in a terminal");
    }
    let remote = &setup.remote;

    let gh = Gh::locate(None).context(
        "cargo-plane manages codespaces through the GitHub CLI; install `gh` \
         (https://cli.github.com) and rerun",
    )?;
    host.status("Found", &format!("gh at {}", gh.path().display()));
    ensure_login(&gh)?;

    let root = setup
        .project_dir
        .as_ref()
        .context("run `cargo plane setup codespace` inside the project you want a codespace for")?;
    let configured = setup
        .config
        .as_ref()
        .and_then(|c| c.get("codespace"))
        .and_then(Value::as_str);

    let name = match configured {
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
            pick_or_create(&gh, host, root, project)?
        }
    };
    host.save_config(&json!({ "codespace": name }))?;

    provision(host)
}

fn ensure_login(gh: &Gh) -> Result<()> {
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
fn pick_or_create(gh: &Gh, host: &dyn Host, root: &Path, project: &str) -> Result<String> {
    let default_repo = gh
        .command(&[
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
        .filter(|r| !r.is_empty());
    let repo = host.ask(
        "Repository to create the codespace from (owner/repo)",
        default_repo.as_deref(),
    )?;

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

    create(gh, host, &repo, project)
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

fn create(gh: &Gh, host: &dyn Host, repo: &str, project: &str) -> Result<String> {
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
    let machine = loop {
        let name = host.ask(
            "Machine type (more CPUs build faster, use up your quota faster)",
            default_machine,
        )?;
        if machines.is_empty() || machines.iter().any(|m| m.name == name) {
            break name;
        }
        host.warn(&format!("unknown machine type `{name}`"));
    };
    let idle_minutes = loop {
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
    if !host.confirm("Create this codespace?", false)? {
        bail!("aborted");
    }

    // gh may ask questions (which dev container to use), so let it talk to
    // the terminal and find the new codespace afterwards.
    let before: BTreeSet<String> = gh.codespaces(repo)?.into_iter().map(|c| c.name).collect();
    host.status(
        "Creating",
        &format!("codespace for {repo} (takes a minute or two)"),
    );
    gh.interactive(&[
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
    ])?;
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
fn provision(host: &dyn Host) -> Result<()> {
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
        if !host.confirm(question, true)? {
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
        if !host.confirm(question, true)? {
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
