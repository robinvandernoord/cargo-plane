mod artifacts;
mod cargo_args;
mod config;
mod host;
mod output;
mod project;
mod provider;
mod remote;
mod ui;
mod util;

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};

use crate::artifacts::Artifact;
use crate::cargo_args::{Invocation, Selector};
use crate::config::{Config, PROJECT_FILE};
use crate::host::SetupHost;
use crate::project::{Project, Stdout};
use crate::remote::{Conn, Remote};

const LIFECYCLE: &[&str] = &["takeoff", "land", "status", "purge"];

const USAGE: &str = "\
Cargo, but in the cloud(s): run cargo on a remote machine.

Usage: cargo plane [--remote <name>] <cargo command> [args...]
       cargo plane [--remote <name>] exec <command> [args...]
       cargo plane [--remote <name>] shell
       cargo plane takeoff | land | status | purge [<remote>]
       cargo plane [--remote <name>] setup <provider> [--help]

Any cargo command works and runs remotely, with these differences:
  build              also copies binaries and cdylibs/staticlibs back
  run                builds remotely, runs the binary locally
  test --local-run   builds tests remotely, runs them locally

Setup:
  setup fly         install flyctl, log in and create a Fly.io build machine
  setup codespace   create a GitHub Codespace (or pick one) to build on
  setup <name>      set up a remote with the `cargo-plane-provider-<name>` plugin

Lifecycle:
  takeoff   start the remote and connect
  land      disconnect and stop the remote
  status    show the remote's state

Other commands, in the project's directory on the remote (synced first):
  exec      run any command, e.g. `cargo plane exec rustup component add miri`
  shell     open an interactive shell
  purge     permanently delete what `setup` created (asks first)

Options:
  --remote <name>   remote to use (default: CARGO_PLANE_REMOTE, then the config)
  -h, --help        show this help
  -V, --version     show the version

Config: .cargo/plane.toml in the project, ~/.config/cargo-plane/config.toml
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            ui::error(format!("{err:#}"));
            ExitCode::from(101)
        }
    }
}

fn run() -> Result<ExitCode> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Invoked by cargo as `cargo-plane plane ...`.
    if args.first().is_some_and(|a| a == "plane") {
        args.remove(0);
    }

    let mut remote_name = std::env::var("CARGO_PLANE_REMOTE")
        .ok()
        .filter(|s| !s.is_empty());
    while let Some(arg) = args.first() {
        if arg == "--remote" {
            args.remove(0);
            if args.is_empty() {
                bail!("--remote needs a value");
            }
            remote_name = Some(args.remove(0));
        } else if let Some(name) = arg.strip_prefix("--remote=") {
            remote_name = Some(name.to_owned());
            args.remove(0);
        } else if arg == "-h" || arg == "--help" {
            print!("{USAGE}");
            return Ok(ExitCode::SUCCESS);
        } else if arg == "-V" || arg == "--version" {
            println!("cargo-plane {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitCode::SUCCESS);
        } else {
            break;
        }
    }
    if args.is_empty() {
        print!("{USAGE}");
        return Ok(ExitCode::FAILURE);
    }

    if args[0] == "setup" {
        return match args.get(1).map(String::as_str) {
            None => {
                print!("{}", setup_usage());
                Ok(ExitCode::FAILURE)
            }
            Some("-h" | "--help" | "help") => {
                print!("{}", setup_usage());
                Ok(ExitCode::SUCCESS)
            }
            Some(flag) if flag.starts_with('-') => {
                bail!("unknown option `{flag}` for `setup`\n\n{}", setup_usage())
            }
            Some(provider) => {
                setup(provider, remote_name.as_deref(), &args[2..])?;
                Ok(ExitCode::SUCCESS)
            }
        };
    }

    // Lifecycle commands take the remote as an optional argument too:
    // `cargo plane purge fly` is `cargo plane --remote fly purge`.
    if LIFECYCLE.contains(&args[0].as_str()) {
        match &args[1..] {
            [] => {}
            [name] => match &remote_name {
                Some(other) if other != name => {
                    bail!("conflicting remotes: `--remote {other}` and `{name}`")
                }
                _ => remote_name = Some(name.clone()),
            },
            [_, extra, ..] => bail!(
                "unexpected argument `{extra}`; usage: cargo plane {} [remote]",
                args[0]
            ),
        }
    }

    let config = Config::load(&std::env::current_dir()?)?;
    let remote = Remote::new(config.select(remote_name.as_deref())?)?;

    match args[0].as_str() {
        "takeoff" => {
            let conn = remote.takeoff()?;
            ui::status(
                "Connected",
                format!("to {} ({})", remote.name, conn.label()),
            );
            Ok(ExitCode::SUCCESS)
        }
        "land" => {
            remote.land()?;
            ui::status("Landed", &remote.name);
            Ok(ExitCode::SUCCESS)
        }
        "purge" => {
            if remote.purge()? {
                ui::status("Purged", &remote.name);
            } else {
                ui::status(
                    "Kept",
                    format!("{} (confirmation didn't match)", remote.name),
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        "status" => {
            println!("{}: {}", remote.name, remote.status()?);
            Ok(ExitCode::SUCCESS)
        }
        "exec" => exec(&remote, &args[1..]),
        "shell" => shell(&remote, &args[1..]),
        _ => cargo(&remote, args),
    }
}

fn setup_usage() -> String {
    let mut usage = String::from(
        "Usage: cargo plane [--remote <name>] setup <provider> [options]\n\n\
         Creates what a remote needs and adds it to the project's .cargo/plane.toml.\n\
         `cargo plane setup <provider> --help` shows a provider's options.\n",
    );
    let builtins: Vec<_> = provider::builtin_setups().collect();
    if !builtins.is_empty() {
        usage += "\nBuilt in:\n";
        for (name, creates) in builtins {
            usage += &format!("  {name:<12} {creates}\n");
        }
    }
    let plugins = provider::plugins();
    if plugins.is_empty() {
        usage += "\nNo other providers on PATH (plugins are named cargo-plane-provider-<name>)\n";
    } else {
        usage += "\nPlugins on PATH:\n";
        for name in plugins {
            usage += &format!("  {name}\n");
        }
    }
    usage
}

/// `cargo plane setup <provider>`: the provider creates what it needs and
/// saves the remote's config; then we check that cargo runs there.
fn setup(provider_name: &str, remote_name: Option<&str>, args: &[String]) -> Result<()> {
    let provider = provider::lookup(provider_name)?;
    let remote = remote_name.unwrap_or(provider_name);
    let project_dir = project::workspace_root(None).ok();
    let existing = match &project_dir {
        Some(dir) => config::project_remote(&dir.join(PROJECT_FILE), remote)?,
        None => None,
    };
    if let Some(table) = &existing {
        let configured = table
            .get("provider")
            .and_then(toml::Value::as_str)
            .unwrap_or(remote);
        if configured != provider_name {
            bail!(
                "remote `{remote}` already uses provider `{configured}`; pick another name \
                 with `cargo plane --remote <name> setup {provider_name}`"
            );
        }
    }
    let setup = cargo_plane_provider::Setup {
        remote: remote.into(),
        project_dir: project_dir.clone(),
        config: existing.map(serde_json::to_value).transpose()?,
        args: args.to_vec(),
    };
    let host = SetupHost::new(remote, provider_name, project_dir);
    provider.setup(&setup, &host)?;
    host.finish()
}

/// The synced project around the current directory, if we're in one.
fn synced_project(conn: &Conn) -> Result<Option<Project>> {
    match Project::locate(&[], conn) {
        Ok(project) => {
            project.sync(conn)?;
            Ok(Some(project))
        }
        // Outside a workspace there's nothing to sync; use the remote $HOME.
        Err(_) => Ok(None),
    }
}

/// `cargo plane exec <command>`: any command, in the project's directory.
fn exec(remote: &Remote, args: &[String]) -> Result<ExitCode> {
    let args = match args {
        [dashes, rest @ ..] if dashes == "--" => rest,
        args => args,
    };
    let [program, args @ ..] = args else {
        bail!("usage: cargo plane exec <command> [args...]");
    };
    let conn = remote.connect()?;
    match synced_project(&conn)? {
        Some(project) => {
            let outcome = project.run(&conn, program, args, Stdout::Passthrough)?;
            Ok(exit_code(outcome.code))
        }
        None => {
            let command = util::shell_join(std::iter::once(program).chain(args));
            let status = conn.shell(&command)?.status()?;
            Ok(exit_code(status.code().unwrap_or(1)))
        }
    }
}

/// `cargo plane shell`: an interactive shell in the project's directory.
fn shell(remote: &Remote, args: &[String]) -> Result<ExitCode> {
    if let Some(arg) = args.first() {
        bail!("unexpected argument `{arg}`; to run a command, use `cargo plane exec`");
    }
    let conn = remote.connect()?;
    let mut script = String::new();
    if let Some(project) = synced_project(&conn)? {
        script += &format!(
            "cd {} || exit 1\n",
            util::shell_quote(&project.remote_cwd())
        );
    }
    script += "exec \"${SHELL:-/bin/sh}\"\n";
    let status = conn.interactive(&script)?.status()?;
    Ok(exit_code(status.code().unwrap_or(1)))
}

fn cargo(remote: &Remote, args: Vec<String>) -> Result<ExitCode> {
    // Formatting is cheap, and done remotely the next sync would undo it.
    if cargo_args::subcommand(&args) == Some("fmt") {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo)
            .args(&args)
            .status()
            .context("running cargo fmt")?;
        return Ok(exit_code(status.code().unwrap_or(1)));
    }
    ui::set_quiet(cargo_args::is_quiet(&args));
    let conn = remote.connect()?;
    let project = Project::locate(&args, &conn)?;
    project.sync(&conn)?;

    match cargo_args::parse(args) {
        Invocation::Passthrough(args) => {
            let outcome = project.cargo(&conn, &args, Stdout::Passthrough)?;
            Ok(exit_code(outcome.code))
        }
        Invocation::Build(call) => {
            let outcome = project.cargo(
                &conn,
                &call.args,
                Stdout::Artifacts {
                    echo: call.user_json,
                },
            )?;
            let files: Vec<_> = outcome
                .artifacts
                .iter()
                .flat_map(Artifact::deliverables)
                .collect();
            project.fetch(&conn, &files)?;
            Ok(exit_code(outcome.code))
        }
        Invocation::Run {
            build,
            program_args,
            bin,
        } => {
            let outcome = project.cargo(
                &conn,
                &build.args,
                Stdout::Artifacts {
                    echo: build.user_json,
                },
            )?;
            if outcome.code != 0 {
                return Ok(exit_code(outcome.code));
            }
            let artifact = pick_binary(&outcome.artifacts, bin.as_ref(), &build.args)?;
            run_local(&project, &conn, artifact, &program_args)
        }
        Invocation::TestLocal {
            build,
            test_args,
            no_fail_fast,
        } => {
            let outcome = project.cargo(
                &conn,
                &build.args,
                Stdout::Artifacts {
                    echo: build.user_json,
                },
            )?;
            if outcome.code != 0 {
                return Ok(exit_code(outcome.code));
            }
            test_local(
                &project,
                &conn,
                &outcome.artifacts,
                &test_args,
                no_fail_fast,
            )
        }
    }
}

/// Which binary `cargo run` would have picked.
fn pick_binary<'a>(
    artifacts: &'a [Artifact],
    selector: Option<&Selector>,
    args: &[String],
) -> Result<&'a Artifact> {
    let package =
        cargo_args::flag_value(args, "--package").or_else(|| cargo_args::flag_value(args, "-p"));
    let candidates: Vec<&Artifact> = artifacts
        .iter()
        .filter(|a| a.executable.is_some() && !a.profile.test)
        .filter(|a| match selector {
            Some(Selector::Bin(name)) => a.has_kind("bin") && &a.target.name == name,
            Some(Selector::Example(name)) => a.has_kind("example") && &a.target.name == name,
            None => a.has_kind("bin"),
        })
        .filter(|a| package.is_none_or(|p| a.package_name() == p))
        .collect();

    match candidates.as_slice() {
        [] => bail!("no binary to run was built"),
        [one] => Ok(one),
        many => {
            // Respect `default-run` when all candidates come from one package.
            let manifest = &many[0].manifest_path;
            if many.iter().all(|a| &a.manifest_path == manifest)
                && let Some(default) = default_run(manifest)?
                && let Some(found) = many.iter().find(|a| a.target.name == default)
            {
                return Ok(found);
            }
            let names: Vec<_> = many.iter().map(|a| a.target.name.as_str()).collect();
            bail!(
                "`cargo run` could not determine which binary to run; use `--bin` (available: {})",
                names.join(", ")
            )
        }
    }
}

fn default_run(manifest: &Path) -> Result<Option<String>> {
    let text = std::fs::read_to_string(manifest)?;
    let manifest: toml::Table = toml::from_str(&text)?;
    Ok(manifest
        .get("package")
        .and_then(|p| p.get("default-run"))
        .and_then(|d| d.as_str())
        .map(str::to_owned))
}

fn run_local(
    project: &Project,
    conn: &Conn,
    artifact: &Artifact,
    args: &[String],
) -> Result<ExitCode> {
    let exe = artifact.executable.as_ref().unwrap();
    project.fetch(conn, std::slice::from_ref(exe))?;
    let shown = exe.strip_prefix(&project.local_root).unwrap_or(exe);
    ui::status(
        "Running",
        format!(
            "`{}`",
            util::shell_join(
                std::iter::once(shown.display().to_string()).chain(args.iter().cloned())
            )
        ),
    );
    let err = local_command(artifact, exe).args(args).exec();
    Err(err).with_context(|| format!("running {}", exe.display()))
}

fn test_local(
    project: &Project,
    conn: &Conn,
    artifacts: &[Artifact],
    args: &[String],
    no_fail_fast: bool,
) -> Result<ExitCode> {
    let mut tests: Vec<&Artifact> = artifacts
        .iter()
        .filter(|a| a.profile.test && a.executable.is_some())
        .collect();
    // Like cargo: unit tests first, then the rest by name.
    tests.sort_by_key(|a| (!is_unittest(a), a.target.name.clone()));
    let exes: Vec<_> = tests.iter().filter_map(|a| a.executable.clone()).collect();
    project.fetch(conn, &exes)?;

    let mut failed = Vec::new();
    for test in tests {
        let exe = test.executable.as_ref().unwrap();
        let src = test
            .target
            .src_path
            .strip_prefix(test.package_dir())
            .unwrap_or(&test.target.src_path);
        let shown = exe.strip_prefix(&project.local_root).unwrap_or(exe);
        let kind = if is_unittest(test) { "unittests " } else { "" };
        ui::status(
            "Running",
            format!("{kind}{} ({})", src.display(), shown.display()),
        );
        let status = local_command(test, exe)
            .args(args)
            .current_dir(test.package_dir())
            .status()
            .with_context(|| format!("running {}", exe.display()))?;
        if !status.success() {
            failed.push(test.target.name.clone());
            if !no_fail_fast {
                break;
            }
        }
    }
    ui::status("Note", "doctests are not run with --local-run");
    if failed.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        ui::error(format!("test failed: {}", failed.join(", ")));
        Ok(ExitCode::from(101))
    }
}

fn is_unittest(artifact: &Artifact) -> bool {
    [
        "lib",
        "bin",
        "rlib",
        "cdylib",
        "dylib",
        "staticlib",
        "proc-macro",
    ]
    .iter()
    .any(|kind| artifact.has_kind(kind))
}

/// The environment cargo sets for `cargo run` / `cargo test` binaries.
fn local_command(artifact: &Artifact, exe: &Path) -> Command {
    let mut cmd = Command::new(exe);
    cmd.env("CARGO_MANIFEST_DIR", artifact.package_dir())
        .env("CARGO_MANIFEST_PATH", &artifact.manifest_path)
        .env("CARGO_PKG_NAME", artifact.package_name());
    cmd
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
