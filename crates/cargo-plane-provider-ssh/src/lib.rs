//! A machine that's always on and reachable over SSH: a VPS, a desktop at home.

mod setup;

use anyhow::{Context, Result, bail};
use cargo_plane_provider::{Endpoint, Host, Provider, Setup};
use serde::Deserialize;
use serde_json::json;

const SETUP_USAGE: &str = "\
Usage: cargo plane [--remote <name>] setup ssh [host] [--user <name>] [--port <port>] [--yes]

Configure an existing SSH host (a hostname or ~/.ssh/config alias).
Without a host argument, asks for one in a terminal. User and port default to
your SSH configuration. Saves .cargo/plane.toml, then checks the build tools.
Offers to install missing rsync, a C compiler and Rust via rustup. System
packages need root or passwordless sudo. Use --yes to accept installation
without prompts. Supported package managers: apt-get, dnf, yum, apk, pacman,
and zypper. Other systems can use tools installed manually.
Safe to rerun; existing settings are preserved unless overridden.
";

#[derive(Deserialize)]
pub struct Config {
    /// Hostname or `~/.ssh/config` alias.
    host: String,
    user: Option<String>,
    port: Option<u16>,
}

pub struct Ssh;

impl Provider for Ssh {
    type Config = Config;

    fn setup(&self, setup: &Setup, host: &dyn Host) -> Result<()> {
        if setup.args.iter().any(|arg| arg == "-h" || arg == "--help") {
            print!("{SETUP_USAGE}");
            return Ok(());
        }
        let mut update = json!({});
        let mut yes = false;
        let mut args = setup.args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--yes" | "-y" => yes = true,
                "--user" => {
                    let user = args.next().context("--user needs a value")?;
                    if user.is_empty() || user.starts_with('-') {
                        bail!("--user needs a nonempty SSH username");
                    }
                    update["user"] = json!(user);
                }
                "--port" => {
                    let port: u16 = args
                        .next()
                        .context("--port needs a value")?
                        .parse()
                        .context("--port must be a number between 1 and 65535")?;
                    if port == 0 {
                        bail!("--port must be a number between 1 and 65535");
                    }
                    update["port"] = json!(port);
                }
                name if !name.starts_with('-') && update.get("host").is_none() => {
                    update["host"] = json!(name);
                }
                other => bail!("unexpected argument `{other}` for `setup ssh`\n\n{SETUP_USAGE}"),
            }
        }
        if update.get("host").is_none() {
            let existing = setup
                .config
                .as_ref()
                .and_then(|config| config.get("host"))
                .and_then(serde_json::Value::as_str);
            let name = if host.interactive() {
                host.ask("SSH hostname or alias", existing)?
            } else {
                existing
                    .context(
                        "provide a host with `cargo plane setup ssh <host>`, or run in a terminal",
                    )?
                    .to_owned()
            };
            update["host"] = json!(name);
        }
        let name = update["host"].as_str().unwrap();
        if name.trim().is_empty() || name.starts_with('-') {
            bail!("provide a nonempty SSH hostname or alias");
        }
        host.save_config(&update)?;
        setup::provision(host, yes)
    }

    fn up(&self, config: &Config, _: &dyn Host) -> Result<Endpoint> {
        Ok(Endpoint {
            user: config.user.clone(),
            port: config.port,
            ..Endpoint::new(&config.host)
        })
    }

    fn down(&self, _: &Config, _: &dyn Host) -> Result<()> {
        Ok(())
    }

    fn status(&self, config: &Config, _: &dyn Host) -> Result<String> {
        Ok(format!("static SSH host `{}`", config.host))
    }
}
