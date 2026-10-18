//! Direct local execution, without a remote endpoint or workspace sync.

use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use cargo_plane_provider::{Host, Setup};

use crate::config::RemoteConfig;
use crate::{cargo_args, ui};

pub fn setup(setup: &Setup, host: &dyn Host) -> Result<()> {
    const USAGE: &str = "Usage: cargo plane [--remote <name>] setup local\n\n\
        Adds local execution to the project's .cargo/plane.toml. Safe to rerun.\n";
    match setup.args.as_slice() {
        [] => host.save_config(&serde_json::json!({})),
        [help] if help == "-h" || help == "--help" => {
            print!("{USAGE}");
            Ok(())
        }
        [other, ..] => bail!("unknown option `{other}` for `setup local`\n\n{USAGE}"),
    }
}

pub fn run(config: &RemoteConfig, mut args: Vec<String>) -> Result<ExitCode> {
    let mut command = match args[0].as_str() {
        "takeoff" | "land" => {
            ui::status(
                "Local",
                format!("{}: nothing to start or stop", config.name),
            );
            return Ok(ExitCode::SUCCESS);
        }
        "status" => {
            println!("{}: local execution (no remote machine)", config.name);
            return Ok(ExitCode::SUCCESS);
        }
        "purge" => bail!("local execution creates no resources, so there's nothing to purge"),
        "exec" => {
            let args = match &args[1..] {
                [dashes, rest @ ..] if dashes == "--" => rest,
                args => args,
            };
            let [program, args @ ..] = args else {
                bail!("usage: cargo plane exec <command> [args...]");
            };
            let mut command = Command::new(program);
            command.args(args);
            command
        }
        "shell" => {
            if let Some(arg) = args.get(1) {
                bail!("unexpected argument `{arg}`; to run a command, use `cargo plane exec`");
            }
            let shell: std::ffi::OsString = config
                .common
                .env
                .get("SHELL")
                .filter(|s| !s.is_empty())
                .map(Into::into)
                .or_else(|| std::env::var_os("SHELL").filter(|s| !s.is_empty()))
                .unwrap_or_else(|| "/bin/sh".into());
            Command::new(shell)
        }
        _ => {
            // Plane's test flag is redundant locally. Preserve everything after
            // `--`, where the same spelling can be an argument to the test binary.
            if matches!(cargo_args::subcommand(&args), Some("test" | "t")) {
                let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
                for i in (0..end).rev() {
                    if args[i] == "--local-run" {
                        args.remove(i);
                    }
                }
            }
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let mut command = Command::new(cargo);
            command.args(args);
            command
        }
    };
    command.envs(&config.common.env);
    // Replace Plane so terminal access, signals and exit status belong directly
    // to the local command, just as when invoked without Plane.
    Err(command.exec()).context("running local command")
}
