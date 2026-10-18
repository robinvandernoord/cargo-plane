//! A Fly.io Machine with a volume for `target/` and the cargo registry.
//! Started on demand; it stops itself when idle (see `image/`).
//! `cargo plane setup fly` creates everything.
//!
//! Unless `ssh_host` is set (e.g. via WireGuard: `<machine>.vm.<app>.internal`),
//! SSH goes through a background `fly proxy` on `127.0.0.1:<proxy_port>`.

mod flyctl;
mod setup;

use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cargo_plane_provider::{Endpoint, Host, Provider, Setup};
use serde::Deserialize;

use self::flyctl::{Flyctl, Machine};

#[derive(Deserialize)]
pub struct Config {
    app: String,
    /// Machine ID; by default the machine `setup` tagged as builder.
    machine: Option<String>,
    proxy_port: Option<u16>,
    ssh_host: Option<String>,
    #[serde(default = "default_user")]
    user: String,
    /// Path to flyctl; found automatically when unset.
    flyctl: Option<String>,
    /// Set by frontends that provision a Docker host rather than a Rust host.
    build_environment: Option<String>,
}

fn default_user() -> String {
    "root".into()
}

/// Paths used by the image in `image/`.
const DATA_DIR: &str = "/data";
/// Our sshd's port; Fly's own SSH server (for `fly ssh console`) has 22.
const SSH_PORT: u16 = 2222;

pub struct Fly;

impl Config {
    fn flyctl(&self) -> Result<Flyctl> {
        Flyctl::require(self.flyctl.as_deref())
    }

    fn machine(&self, fly: &Flyctl) -> Result<Machine> {
        fly.builder(&self.app, self.machine.as_deref())?
            .with_context(|| {
                format!(
                    "no cargo-plane machine found in Fly app `{}`; run `cargo plane setup fly`",
                    self.app
                )
            })
    }

    /// Stable per app, so several Fly remotes don't fight over one port.
    fn proxy_addr(&self) -> SocketAddr {
        let port = self.proxy_port.unwrap_or(
            20000 + (cargo_plane_provider::stable_hash(self.app.as_bytes()) % 10000) as u16,
        );
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn proxy_is_up(&self) -> bool {
        TcpStream::connect_timeout(&self.proxy_addr(), Duration::from_millis(300)).is_ok()
    }

    fn pid_file(&self) -> Result<PathBuf> {
        Ok(cargo_plane_provider::cache_dir()?.join(format!("fly-proxy-{}.pid", self.app)))
    }

    fn ensure_proxy(&self, fly: &Flyctl, machine: &Machine) -> Result<()> {
        if self.proxy_is_up() {
            return Ok(());
        }
        let dir = cargo_plane_provider::cache_dir()?;
        fs::create_dir_all(&dir)?;
        let log_path = dir.join(format!("fly-proxy-{}.log", self.app));
        let log = fs::File::create(&log_path)?;
        let ports = format!("{}:{SSH_PORT}", self.proxy_addr().port());
        let target = format!("{}.vm.{}.internal", machine.id, self.app);
        let child = fly
            .command(&["proxy", &ports, &target, "--app", &self.app])
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            // Own process group, so Ctrl-C in the terminal doesn't kill it.
            .process_group(0)
            .spawn()
            .context("starting `flyctl proxy`")?;
        fs::write(self.pid_file()?, child.id().to_string())?;

        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.proxy_is_up() {
            if Instant::now() > deadline {
                bail!(
                    "`flyctl proxy` did not come up on {} (see {})",
                    self.proxy_addr(),
                    log_path.display()
                );
            }
            sleep(Duration::from_millis(250));
        }
        Ok(())
    }

    fn stop_proxy(&self) -> Result<()> {
        let pid_file = self.pid_file()?;
        if let Ok(pid) = fs::read_to_string(&pid_file) {
            // The pid may be stale; failing to kill it is fine.
            let _ = Command::new("kill")
                .arg(pid.trim())
                .stderr(Stdio::null())
                .status();
            let _ = fs::remove_file(pid_file);
        }
        Ok(())
    }
}

impl Provider for Fly {
    type Config = Config;

    fn up(&self, config: &Config, host: &dyn Host) -> Result<Endpoint> {
        let fly = config.flyctl()?;
        let mut machine = config.machine(&fly)?;
        if config.build_environment.as_deref() == Some("docker") && !machine.docker() {
            bail!(
                "this Fly machine was provisioned for Cargo; run Docker setup with a separate app, or explicitly rebuild its image"
            );
        }
        if machine.state != "started" {
            host.status(
                "Starting",
                &format!("Fly machine {} ({})", machine.id, machine.state),
            );
            fly.output(&["machine", "start", &machine.id, "--app", &config.app])?;
            let deadline = Instant::now() + Duration::from_secs(60);
            while machine.state != "started" {
                if Instant::now() > deadline {
                    bail!("Fly machine {} did not start within 60s", machine.id);
                }
                sleep(Duration::from_secs(1));
                machine = config.machine(&fly)?;
            }
        }

        let mut ssh_options = Vec::new();
        let (host, port) = match &config.ssh_host {
            Some(host) => (host.clone(), SSH_PORT),
            None => {
                config.ensure_proxy(&fly, &machine)?;
                // All proxied machines share 127.0.0.1; keep their host keys apart.
                ssh_options.extend([
                    "-o".into(),
                    format!("HostKeyAlias=cargo-plane-fly-{}", config.app),
                    "-o".into(),
                    "StrictHostKeyChecking=accept-new".into(),
                ]);
                ("127.0.0.1".into(), config.proxy_addr().port())
            }
        };

        Ok(Endpoint {
            host,
            user: Some(config.user.clone()),
            port: Some(port),
            ssh_options,
            remote_dir: Some(format!("{DATA_DIR}/cargo-plane")),
            // sshd doesn't pass the image's ENV on, so set the toolchain up here.
            env: [
                ("CARGO_HOME", format!("{DATA_DIR}/cargo")),
                ("RUSTUP_HOME", format!("{DATA_DIR}/rustup")),
                (
                    "PATH",
                    "/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                        .into(),
                ),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
            prelude: None,
        })
    }

    fn down(&self, config: &Config, host: &dyn Host) -> Result<()> {
        config.stop_proxy()?;
        let fly = config.flyctl()?;
        let machine = config.machine(&fly)?;
        if machine.state == "started" {
            host.status("Stopping", &format!("Fly machine {}", machine.id));
            fly.output(&["machine", "stop", &machine.id, "--app", &config.app])?;
        }
        Ok(())
    }

    fn status(&self, config: &Config, _: &dyn Host) -> Result<String> {
        let proxy = match (&config.ssh_host, config.proxy_is_up()) {
            (Some(host), _) => format!("direct SSH to {host}"),
            (None, true) => format!("proxy on {}", config.proxy_addr()),
            (None, false) => "proxy not running".into(),
        };
        let machine = config.machine(&config.flyctl()?)?;
        Ok(format!(
            "Fly machine {} in app {} ({}): {} ({proxy})",
            machine.id, config.app, machine.region, machine.state
        ))
    }

    fn purge(&self, config: &Config, host: &dyn Host) -> Result<bool> {
        if !host.interactive() {
            bail!("`cargo plane purge` asks for confirmation; run it in a terminal");
        }
        let fly = config.flyctl()?;
        let app = &config.app;
        if !fly.app_exists(app) {
            host.status("Gone", &format!("Fly app `{app}` doesn't exist (anymore)"));
            config.stop_proxy()?;
            return Ok(true);
        }
        let machines = fly.machines(app)?;
        let volumes: serde_json::Value = fly.json(&["volumes", "list", "--app", app, "--json"])?;

        eprintln!("This permanently deletes the Fly.io app `{app}`, including:");
        for machine in &machines {
            eprintln!(
                "  - machine {} ({}, {})",
                machine.id, machine.region, machine.state
            );
        }
        for volume in volumes.as_array().into_iter().flatten() {
            let get = |key| {
                volume
                    .get(key)
                    .map(|v| v.to_string().trim_matches('"').to_owned())
            };
            eprintln!(
                "  - volume {} ({} GB) and everything on it",
                get("name").unwrap_or_default(),
                get("size_gb").unwrap_or_else(|| "?".into())
            );
        }
        eprintln!("Builds start from scratch if you set it up again.");
        if host.ask("Type the app name to confirm", None)? != *app {
            return Ok(false);
        }

        config.stop_proxy()?;
        host.status("Destroying", &format!("Fly app {app}"));
        fly.interactive(&["apps", "destroy", app, "--yes"])?;
        Ok(true)
    }

    fn boot_timeout(&self, _: &Config) -> Duration {
        Duration::from_secs(60)
    }

    fn setup(&self, setup: &Setup, host: &dyn Host) -> Result<()> {
        setup::setup(setup, host)
    }
}
