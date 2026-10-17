//! A configured remote: its provider, plus a cached SSH connection to it.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::RemoteConfig;
use crate::provider::{self, Endpoint, Provider};
use crate::{ui, util};

const DEFAULT_REMOTE_DIR: &str = ".cache/cargo-plane";

pub struct Remote {
    pub name: String,
    provider: Box<dyn Provider>,
    config: RemoteConfig,
    state_file: PathBuf,
}

/// What `takeoff` learned, cached so later commands skip the provider.
#[derive(Serialize, Deserialize)]
struct State {
    endpoint: Endpoint,
    home: String,
}

/// A reachable remote.
pub struct Conn {
    pub endpoint: Endpoint,
    /// Remote `$HOME`.
    pub home: String,
    control_path: PathBuf,
}

impl Remote {
    pub fn new(config: RemoteConfig) -> Result<Self> {
        let provider = provider::from_config(&config)?;
        // Keyed by the config too, so editing a remote invalidates its cache.
        let key = util::fnv1a(toml::to_string(&config.table)?.as_bytes());
        let state_file = util::cache_dir()?
            .join("state")
            .join(format!("{}-{key:016x}.json", config.name));
        Ok(Self {
            name: config.name.clone(),
            provider,
            config,
            state_file,
        })
    }

    /// Connect using the cached endpoint, falling back to a full `takeoff`.
    pub fn connect(&self) -> Result<Conn> {
        if let Some(state) = self.load_state() {
            let conn = Conn::new(state.endpoint, state.home)?;
            if conn.master_alive() || conn.query_home().is_ok() {
                return Ok(conn);
            }
        }
        self.takeoff()
    }

    pub fn takeoff(&self) -> Result<Conn> {
        let mut endpoint = self.provider.up()?;
        let common = &self.config.common;
        if common.remote_dir.is_some() {
            endpoint.remote_dir = common.remote_dir.clone();
        }
        endpoint.env.extend(common.env.clone());
        endpoint
            .ssh_options
            .extend(common.ssh_options.iter().cloned());

        let mut conn = Conn::new(endpoint, String::new())?;
        let timeout = self.provider.boot_timeout();
        let deadline = Instant::now() + timeout;
        let mut waiting = false;
        conn.home = loop {
            match conn.query_home() {
                Ok(home) => break home,
                // Booting machines refuse connections for a bit; a rejected
                // key won't fix itself.
                Err(err)
                    if Instant::now() < deadline
                        && !err.to_string().contains("Permission denied") =>
                {
                    if !waiting {
                        ui::status(
                            "Waiting",
                            format!("for SSH on {} (up to {}s)", self.name, timeout.as_secs()),
                        );
                        waiting = true;
                    }
                    sleep(Duration::from_secs(1));
                }
                Err(err) => {
                    return Err(err.context(format!(
                        "remote `{}` ({}) is unreachable",
                        self.name, conn.endpoint.host
                    )));
                }
            }
        };

        fs::create_dir_all(self.state_file.parent().unwrap())?;
        let state = State {
            endpoint: conn.endpoint.clone(),
            home: conn.home.clone(),
        };
        fs::write(&self.state_file, serde_json::to_vec_pretty(&state)?)?;
        Ok(conn)
    }

    pub fn land(&self) -> Result<()> {
        if let Some(state) = self.load_state() {
            Conn::new(state.endpoint, state.home)?.close_master();
        }
        let _ = fs::remove_file(&self.state_file);
        self.provider.down()
    }

    /// Delete the provider's resources, our cached state and the config entry.
    pub fn purge(&self) -> Result<bool> {
        let purged = self
            .provider
            .purge()
            .with_context(|| format!("can't purge remote `{}`", self.name))?;
        if !purged {
            return Ok(false);
        }
        if let Some(state) = self.load_state() {
            Conn::new(state.endpoint, state.home)?.close_master();
        }
        let _ = fs::remove_file(&self.state_file);
        crate::config::remove_remote(&self.name)?;
        Ok(true)
    }

    pub fn status(&self) -> Result<String> {
        let mut status = self.provider.status()?;
        match self.load_state() {
            Some(state) => {
                let conn = Conn::new(state.endpoint, state.home)?;
                let reachable = conn.master_alive() || conn.query_home().is_ok();
                status += if reachable {
                    ", reachable"
                } else {
                    ", unreachable"
                };
            }
            None => status += ", not connected",
        }
        Ok(status)
    }

    fn load_state(&self) -> Option<State> {
        let bytes = fs::read(&self.state_file).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

impl Conn {
    fn new(endpoint: Endpoint, home: String) -> Result<Self> {
        let dir = util::cache_dir()?.join("ssh");
        fs::create_dir_all(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            endpoint,
            home,
            control_path: dir.join("%C"),
        })
    }

    pub fn label(&self) -> &str {
        &self.endpoint.host
    }

    /// Absolute remote base dir for synced projects.
    pub fn base_dir(&self) -> String {
        let dir = self
            .endpoint
            .remote_dir
            .as_deref()
            .unwrap_or(DEFAULT_REMOTE_DIR);
        if dir.starts_with('/') {
            dir.trim_end_matches('/').to_owned()
        } else {
            format!(
                "{}/{}",
                self.home.trim_end_matches('/'),
                dir.trim_end_matches('/')
            )
        }
    }

    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.endpoint.env
    }

    pub fn prelude(&self) -> Option<&str> {
        self.endpoint.prelude.as_deref()
    }

    /// `ssh` running a POSIX shell command with the remote's environment set up.
    pub fn shell(&self, command: &str) -> Result<Command> {
        let script = format!("{}{command}", env_prelude(self.prelude(), self.env())?);
        let mut cmd = self.ssh();
        cmd.arg(format!("sh -c {}", util::shell_quote(&script)));
        Ok(cmd)
    }

    /// SSH options, without the destination. One control master per
    /// destination keeps later connections down to a single round trip.
    fn ssh_options(&self) -> Vec<String> {
        let mut opts: Vec<String> = [
            "-o",
            "ControlMaster=auto",
            "-o",
            &format!("ControlPath={}", self.control_path.display()),
            "-o",
            "ControlPersist=10m",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "BatchMode=yes",
        ]
        .map(String::from)
        .to_vec();
        if let Some(port) = self.endpoint.port {
            opts.extend(["-p".into(), port.to_string()]);
        }
        if let Some(user) = &self.endpoint.user {
            opts.extend(["-l".into(), user.clone()]);
        }
        opts.extend(self.endpoint.ssh_options.iter().cloned());
        opts
    }

    pub fn ssh(&self) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.args(self.ssh_options()).arg(&self.endpoint.host);
        cmd
    }

    /// `rsync` with `-e` set up to reuse our SSH connection.
    pub fn rsync(&self) -> Command {
        let mut cmd = Command::new("rsync");
        let ssh = util::shell_join(std::iter::once("ssh".to_owned()).chain(self.ssh_options()));
        cmd.arg("-e").arg(ssh);
        cmd
    }

    /// `host:path` for rsync.
    pub fn rsync_path(&self, path: &str) -> String {
        format!("{}:{path}", self.endpoint.host)
    }

    fn master_alive(&self) -> bool {
        let mut cmd = self.ssh();
        cmd.args(["-O", "check"]);
        quiet_status(cmd)
    }

    fn close_master(&self) {
        let mut cmd = self.ssh();
        cmd.args(["-O", "exit"]);
        quiet_status(cmd);
    }

    /// Connects (starting the control master) and returns the remote `$HOME`.
    fn query_home(&self) -> Result<String> {
        let output = self
            .ssh()
            .args(["-o", "ConnectTimeout=10", "echo \"$HOME\""])
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("running ssh")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("ssh to {} failed: {}", self.endpoint.host, stderr.trim());
        }
        let home = String::from_utf8(output.stdout)?.trim().to_owned();
        if !home.starts_with('/') {
            ui::warn(format!("unexpected remote $HOME: {home:?}"));
        }
        Ok(home)
    }
}

/// Shell lines running the provider's `prelude`, making rustup's toolchain
/// available and exporting `env`.
pub fn env_prelude(prelude: Option<&str>, env: &BTreeMap<String, String>) -> Result<String> {
    let mut script = prelude.unwrap_or_default().to_owned();
    if !script.is_empty() && !script.ends_with('\n') {
        script.push('\n');
    }
    // `.` on a missing file makes dash exit, so check first: the Fly image
    // has no ~/.cargo/env (its toolchain comes from PATH).
    script += "[ -f \"$HOME/.cargo/env\" ] && . \"$HOME/.cargo/env\"\n";
    for (key, value) in env {
        let valid = key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            bail!("invalid environment variable name `{key}`");
        }
        script += &format!("export {key}={}\n", util::shell_quote(value));
    }
    Ok(script)
}

fn quiet_status(mut cmd: Command) -> bool {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
