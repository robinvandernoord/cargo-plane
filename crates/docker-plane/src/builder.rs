use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cargo_plane_provider::{AnyProvider, Endpoint, Host};
use tempfile::TempDir;

use crate::config::Remote;
use crate::host::Terminal;

pub fn provider(name: &str) -> Result<Box<dyn AnyProvider>> {
    match name {
        "ssh" => Ok(Box::new(cargo_plane_provider_ssh::Ssh)),
        "fly" => Ok(Box::new(cargo_plane_provider_fly::Fly)),
        "codespace" => Ok(Box::new(cargo_plane_provider_codespace::Codespace)),
        _ => bail!("unknown Docker provider `{name}`; choose local, ssh, fly or codespace"),
    }
}

pub fn docker() -> Command {
    Command::new(
        std::env::var_os("DOCKER_CLI_PLUGIN_ORIGINAL_CLI_COMMAND")
            .unwrap_or_else(|| OsString::from("docker")),
    )
}

pub fn connect(remote: &Remote) -> Result<Connection> {
    let started = provider(&remote.provider)?.up(&remote.config, &Terminal)?;
    let mut endpoint = started.endpoint;
    if let Some(options) = remote.config.get("ssh_options") {
        let options: Vec<String> = serde_json::from_value(options.clone())
            .context("ssh_options must be an array of strings")?;
        endpoint.ssh_options.extend(options);
    }
    validate_endpoint(&endpoint)?;
    let ssh = find_ssh()?;
    let deadline = Instant::now() + started.boot_timeout;
    loop {
        let output = ssh_command(&ssh, &endpoint, "true")
            .stdin(Stdio::null())
            .output()
            .context("checking remote SSH connection")?;
        if output.status.success() {
            break;
        }
        let error = String::from_utf8_lossy(&output.stderr);
        if Instant::now() >= deadline || error.contains("Permission denied") {
            bail!("SSH to remote `{}` failed: {}", remote.name, error.trim());
        }
        sleep(Duration::from_secs(1));
    }

    // A private, invocation-scoped SSH executable lets Docker's connection
    // helper use provider options such as Codespaces' ProxyCommand and Fly's
    // HostKeyAlias without modifying ~/.ssh/config or the user's Docker context.
    let shim = tempfile::Builder::new().prefix("docker-plane-").tempdir()?;
    let executable = shim.path().join("ssh");
    let script = format!(
        "#!/bin/sh\nexec {} {} \"$@\"\n",
        quote(ssh.to_str().context("SSH path is not UTF-8")?),
        ssh_options(&endpoint)
            .iter()
            .map(|arg| quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    );
    fs::write(&executable, script)?;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(shim.path().to_path_buf()).chain(std::env::split_paths(&path)),
    )?;
    let endpoint_url = endpoint_url(&endpoint);
    let builder = builder_name(remote)?;

    // Closing this process's stdin releases the session. It keeps the Fly idle
    // watchdog from stopping a machine while Buildx is still preparing a build.
    let mut keepalive = ssh_command(
        &ssh,
        &endpoint,
        "printf 'ready\\n'; while IFS= read -r line; do :; done",
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()
    .context("holding remote SSH connection")?;
    let mut ready = String::new();
    let read = BufReader::new(keepalive.stdout.take().unwrap()).read_line(&mut ready);
    if read.is_err() || ready.trim() != "ready" {
        let _ = keepalive.kill();
        let _ = keepalive.wait();
        bail!("could not hold the remote SSH connection open");
    }
    Ok(Connection {
        endpoint,
        endpoint_url,
        builder,
        ssh,
        path,
        _shim: shim,
        keepalive,
    })
}

pub struct Connection {
    endpoint: Endpoint,
    endpoint_url: String,
    pub builder: String,
    ssh: PathBuf,
    path: OsString,
    _shim: TempDir,
    keepalive: Child,
}

impl Connection {
    pub fn ssh_command(&self, command: &str) -> Command {
        ssh_command(&self.ssh, &self.endpoint, command)
    }

    pub fn docker(&self) -> Command {
        let mut command = docker();
        command.env("PATH", &self.path);
        command
    }

    pub fn ensure_builder(&self, globals: &[OsString]) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let output = self
                .ssh_command("docker info >/dev/null")
                .stdin(Stdio::null())
                .output()?;
            if output.status.success() {
                break;
            }
            if Instant::now() >= deadline {
                bail!(
                    "Docker is unavailable on the remote; install/start Docker and allow the SSH user to access it, then rerun setup: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            sleep(Duration::from_secs(1));
        }
        let output = self
            .docker()
            .args(globals)
            .args(["buildx", "inspect", &self.builder])
            .stdin(Stdio::null())
            .output()
            .context("inspecting Buildx builder")?;
        if !output.status.success() {
            Terminal.status("Creating", &format!("Buildx builder {}", self.builder));
            let status = self
                .docker()
                .args(globals)
                .args([
                    "buildx",
                    "create",
                    "--name",
                    &self.builder,
                    "--driver",
                    "docker-container",
                    &self.endpoint_url,
                ])
                .stdout(Stdio::null())
                .status()?;
            // Two simultaneous builds can race to register the same builder.
            if !status.success()
                && !self
                    .docker()
                    .args(globals)
                    .args(["buildx", "inspect", &self.builder])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .status()?
                    .success()
            {
                bail!("could not register Buildx builder ({status})");
            }
        }
        let status = self
            .docker()
            .args(globals)
            .args(["buildx", "inspect", "--bootstrap", &self.builder])
            .stdout(Stdio::null())
            .status()?;
        if !status.success() {
            bail!("starting the remote BuildKit container failed ({status})");
        }
        Ok(())
    }

    pub fn build(
        &self,
        subcommand: &str,
        args: &[OsString],
        globals: &[OsString],
    ) -> Result<ExitStatus> {
        self.ensure_builder(globals)?;
        self.docker()
            .args(globals)
            .args(["buildx", subcommand, "--builder", &self.builder])
            .args(args)
            .status()
            .context("running Docker Buildx")
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        drop(self.keepalive.stdin.take());
        // Bound cleanup if the network went away during the build.
        for _ in 0..20 {
            if self.keepalive.try_wait().ok().flatten().is_some() {
                return;
            }
            sleep(Duration::from_millis(25));
        }
        let _ = self.keepalive.kill();
        let _ = self.keepalive.wait();
    }
}

fn ssh_options(endpoint: &Endpoint) -> Vec<String> {
    let mut options: Vec<String> = [
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    options.extend(endpoint.ssh_options.clone());
    options
}

fn ssh_command(ssh: &Path, endpoint: &Endpoint, command: &str) -> Command {
    let mut process = Command::new(ssh);
    process.arg("-T").args(ssh_options(endpoint));
    if let Some(user) = &endpoint.user {
        process.args(["-l", user]);
    }
    if let Some(port) = endpoint.port {
        process.arg("-p").arg(port.to_string());
    }
    process.arg("--").arg(&endpoint.host).arg(command);
    process
}

fn validate_endpoint(endpoint: &Endpoint) -> Result<()> {
    if endpoint.host.is_empty()
        || endpoint.host.starts_with('-')
        || !endpoint
            .host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-:".contains(&c))
    {
        bail!("provider returned an invalid SSH hostname");
    }
    if endpoint.user.as_ref().is_some_and(|user| {
        user.is_empty()
            || !user
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    }) {
        bail!("provider returned an invalid SSH username");
    }
    if endpoint.port == Some(0) {
        bail!("SSH port must be nonzero");
    }
    Ok(())
}

fn endpoint_url(endpoint: &Endpoint) -> String {
    let mut url = String::from("ssh://");
    if let Some(user) = &endpoint.user {
        url.push_str(user);
        url.push('@');
    }
    if endpoint.host.contains(':') {
        url.push('[');
        url.push_str(&endpoint.host);
        url.push(']');
    } else {
        url.push_str(&endpoint.host);
    }
    if let Some(port) = endpoint.port {
        url.push(':');
        url.push_str(&port.to_string());
    }
    url
}

fn find_ssh() -> Result<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("ssh"))
        .find(|path| {
            fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .map(fs::canonicalize)
        .transpose()?
        .context("ssh not found on PATH")
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn builder_name(remote: &Remote) -> Result<String> {
    let fingerprint = serde_json::to_vec(&(remote.name.as_str(), &remote.config))?;
    Ok(format!(
        "docker-plane-{:016x}",
        cargo_plane_provider::stable_hash(&fingerprint)
    ))
}

/// Remove the local registration after resources are gone, without reconnecting
/// to (or trying to stop containers on) a machine that was just deleted.
pub fn forget_builder(remote: &Remote, globals: &[OsString]) -> Result<()> {
    let name = builder_name(remote)?;
    let status = docker()
        .args(globals)
        .args(["buildx", "rm", "--force", "--keep-daemon", &name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        Terminal.warn(&format!("could not remove local builder {name}; remove it with `docker buildx rm --keep-daemon {name}`"));
    }
    Ok(())
}
