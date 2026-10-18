mod builder;
mod config;
mod host;

use std::ffi::OsString;
use std::process::{ExitCode, ExitStatus};

use anyhow::{Context, Result, bail};
use cargo_plane_provider::{Host, Setup};
use serde_json::json;

use config::Config;
use host::{SetupHost, Terminal};

const USAGE: &str = "\
Docker builds on remote machines.

Usage: docker plane [--remote <name>] build [Docker build arguments...]
       docker plane [--remote <name>] bake [Buildx Bake arguments...]
       docker plane [--remote <name>] setup <local|ssh|fly|codespace> [options...]
       docker plane takeoff | land | status | purge [<remote>]

Also available as docker-plane, without installing the Docker CLI plugin.

Builds use Docker Buildx. Pass --load to import an image into your current
Docker engine, or --push to publish it. Without an output, results stay cached
on the remote. Build and Bake arguments are forwarded unchanged except that
docker-plane owns --builder. The selected Docker context is not changed.

Setup:
  local                use Docker Buildx directly in your current Docker context
  ssh [host]           existing Docker host; supports --user, --port and --yes
  fly                  create a Docker host on Fly with persistent storage
                       supports --rebuild, --dockerfile <path>, --eject <dir>
  codespace            create or pick a Codespace with Docker available

Lifecycle:
  takeoff              start the machine and prepare the Buildx builder
  land                 stop the machine, retaining its caches
  status               report provider state without starting the machine
  purge                delete provider resources after confirmation

Options:
  --remote <name>       override DOCKER_PLANE_REMOTE and the configured default
  -h, --help           show help
  -V, --version        show version

Config: .docker/plane.toml over ~/.config/docker-plane/config.toml
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args
        .first()
        .is_some_and(|a| a == "docker-cli-plugin-metadata")
    {
        println!(
            "{}",
            json!({
                "SchemaVersion": "0.1.0", "Vendor": "docker-plane",
                "Version": env!("CARGO_PKG_VERSION"),
                "ShortDescription": "Docker builds on remote machines",
            })
        );
        return Ok(ExitCode::SUCCESS);
    }
    let globals = docker_globals(&mut args)?;
    let mut selected = std::env::var("DOCKER_PLANE_REMOTE")
        .ok()
        .filter(|name| !name.is_empty());
    while let Some(arg) = args.first() {
        if arg == "--remote" {
            args.remove(0);
            selected = Some(
                args.first()
                    .context("--remote needs a name")?
                    .to_str()
                    .context("remote name must be UTF-8")?
                    .to_owned(),
            );
            args.remove(0);
        } else if let Some(name) = arg.to_str().and_then(|a| a.strip_prefix("--remote=")) {
            selected = Some(name.into());
            args.remove(0);
        } else if arg == "-h" || arg == "--help" {
            print!("{USAGE}");
            return Ok(ExitCode::SUCCESS);
        } else if arg == "-V" || arg == "--version" {
            println!("docker-plane {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitCode::SUCCESS);
        } else {
            break;
        }
    }
    if args.is_empty() {
        print!("{USAGE}");
        return Ok(ExitCode::FAILURE);
    }
    if let Some(name) = &selected {
        config::validate_name(name)?;
    }
    let command = args
        .remove(0)
        .into_string()
        .map_err(|_| anyhow::anyhow!("command must be UTF-8"))?;
    if command == "help" {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    if !matches!(
        command.as_str(),
        "setup" | "build" | "bake" | "takeoff" | "land" | "status" | "purge"
    ) {
        bail!("unknown command `{command}`; run `docker plane --help`");
    }
    if command == "setup" {
        return setup(&args, selected.as_deref(), &globals);
    }
    if !matches!(command.as_str(), "build" | "bake")
        && args.iter().any(|arg| arg == "--help" || arg == "-h")
    {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    if matches!(command.as_str(), "build" | "bake") {
        // Help never starts a cloud machine or requires project configuration.
        if args.iter().any(|a| a == "--help" || a == "-h") {
            return Ok(exit_code(
                builder::docker()
                    .args(&globals)
                    .args(["buildx", &command, "--help"])
                    .status()?,
            ));
        }
        if args
            .iter()
            .take_while(|a| *a != "--")
            .any(|a| a == "--builder" || a.to_str().is_some_and(|a| a.starts_with("--builder=")))
        {
            bail!("docker-plane selects the builder; use --remote before {command}");
        }
    } else if args.len() > 1 {
        bail!("{command} accepts at most one remote name");
    } else if let Some(name) = args.first() {
        if selected.is_some() {
            bail!("select a remote either with --remote or after {command}");
        }
        selected = Some(name.to_str().context("remote name must be UTF-8")?.into());
    }

    let config = Config::load()?;
    let remote = config.select(selected.as_deref())?;
    if remote.provider == "local" {
        return match command.as_str() {
            "build" | "bake" => Ok(exit_code(
                builder::docker()
                    .args(&globals)
                    .args(["buildx", &command])
                    .args(args)
                    .status()?,
            )),
            "purge" => {
                config.remove(&remote.name)?;
                Ok(ExitCode::SUCCESS)
            }
            _ => {
                println!("remote `{}` uses the current Docker context", remote.name);
                Ok(ExitCode::SUCCESS)
            }
        };
    }
    match command.as_str() {
        "build" | "bake" => {
            check_buildx(&globals)?;
            let connection = builder::connect(&remote)?;
            Ok(exit_code(connection.build(&command, &args, &globals)?))
        }
        "takeoff" => {
            check_buildx(&globals)?;
            let connection = builder::connect(&remote)?;
            connection.ensure_builder(&globals)?;
            Terminal.status("Ready", &format!("Buildx builder {}", connection.builder));
            Ok(ExitCode::SUCCESS)
        }
        "status" => {
            println!(
                "{}: {}",
                remote.name,
                builder::provider(&remote.provider)?.status(&remote.config, &Terminal)?
            );
            Ok(ExitCode::SUCCESS)
        }
        "land" => {
            builder::provider(&remote.provider)?.down(&remote.config, &Terminal)?;
            Ok(ExitCode::SUCCESS)
        }
        "purge" => {
            if remote.provider == "ssh" {
                // SSH providers own no machine. Forgetting their configuration
                // must not delete shared Docker data on the host.
                if Terminal.confirm(
                    &format!("Remove Docker remote `{}` from configuration?", remote.name),
                    false,
                )? {
                    config.remove(&remote.name)?;
                    builder::forget_builder(&remote, &globals)?;
                }
            } else if builder::provider(&remote.provider)?.purge(&remote.config, &Terminal)? {
                config.remove(&remote.name)?;
                builder::forget_builder(&remote, &globals)?;
            }
            Ok(ExitCode::SUCCESS)
        }
        _ => unreachable!(),
    }
}

fn setup(args: &[OsString], selected: Option<&str>, globals: &[OsString]) -> Result<ExitCode> {
    let Some(provider) = args.first().and_then(|p| p.to_str()) else {
        print!("{USAGE}");
        return Ok(ExitCode::FAILURE);
    };
    if !matches!(provider, "local" | "ssh" | "fly" | "codespace") {
        bail!("unknown Docker provider `{provider}`; choose local, ssh, fly or codespace");
    }
    if args[1..].iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let config = Config::load()?;
    let name = selected.unwrap_or(provider);
    config::validate_name(name)?;
    let mut existing = config.existing(name).unwrap_or_else(|| json!({}));
    if !existing.is_object() {
        bail!("remotes.{name} must be a table");
    }
    if let Some(old) = existing.get("provider").and_then(serde_json::Value::as_str)
        && old != provider
    {
        bail!("remote `{name}` already uses provider `{old}`; choose another --remote name");
    }
    existing["build_environment"] = "docker".into();
    if provider == "local" {
        if args.len() != 1 {
            bail!("setup local takes no arguments");
        }
        check_buildx(globals)?;
        config.save(name, provider, &existing)?;
        Terminal.status("Ready", "local Docker execution configured");
        return Ok(ExitCode::SUCCESS);
    }
    let setup = Setup {
        remote: name.into(),
        project_dir: Some(config.root().to_path_buf()),
        config: Some(existing),
        args: args[1..]
            .iter()
            .map(|a| {
                a.to_str()
                    .context("setup arguments must be UTF-8")
                    .map(String::from)
            })
            .collect::<Result<_>>()?,
    };
    let eject = provider == "fly" && setup.args.iter().any(|a| a == "--eject");
    if !eject {
        check_buildx(globals)?;
    }
    let host = SetupHost::new(&config, name, provider, globals);
    builder::provider(provider)?.setup(&setup, &host)?;
    host.finish()?;
    Ok(ExitCode::SUCCESS)
}

fn check_buildx(globals: &[OsString]) -> Result<()> {
    let output = builder::docker()
        .args(globals)
        .args(["buildx", "version"])
        .output()
        .context("Docker CLI not found; install Docker CLI and Buildx locally")?;
    if !output.status.success() {
        bail!(
            "Docker Buildx is required locally: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn exit_code(status: ExitStatus) -> ExitCode {
    use std::os::unix::process::ExitStatusExt;
    ExitCode::from(
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)) as u8,
    )
}

/// Docker invokes plugins with its original global flags followed by `plane`.
/// Standalone invocations start directly with our flags or subcommand.
fn docker_globals(args: &mut Vec<OsString>) -> Result<Vec<OsString>> {
    if args.first().is_some_and(|a| a == "plane") {
        args.remove(0);
        return Ok(Vec::new());
    }
    if std::env::var_os("DOCKER_CLI_PLUGIN_ORIGINAL_CLI_COMMAND").is_none() {
        return Ok(Vec::new());
    }
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg == "plane" {
            let globals = args.drain(..index).collect();
            args.remove(0);
            return Ok(globals);
        }
        let flag = arg.to_str().context("Docker global flags must be UTF-8")?;
        if matches!(
            flag,
            "--config"
                | "--context"
                | "-c"
                | "--host"
                | "-H"
                | "--log-level"
                | "-l"
                | "--tlscacert"
                | "--tlscert"
                | "--tlskey"
        ) {
            index += 2;
        } else if flag.starts_with('-') {
            index += 1;
        } else {
            bail!("expected the plane subcommand after Docker global flags");
        }
    }
    bail!("Docker plugin invocation is missing the plane subcommand")
}
