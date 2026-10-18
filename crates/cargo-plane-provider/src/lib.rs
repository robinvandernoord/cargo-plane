//! Providers for [cargo-plane](https://crates.io/crates/cargo-plane). A
//! provider starts and stops a remote machine and says how to reach it over
//! SSH; cargo-plane does everything else (syncing, running cargo, fetching
//! artifacts).
//!
//! docker-plane also uses these providers for lifecycle and SSH connectivity,
//! then delegates image builds to Buildx. Its setup configuration includes
//! `build_environment = "docker"`; providers can inspect [`Setup::docker`] to
//! choose the appropriate host image or tool checks. Existing Cargo callers
//! and external-provider protocol messages remain compatible.
//!
//! A plugin is a binary named `cargo-plane-provider-<name>` on PATH. Remotes
//! with `provider = "<name>"` use it:
//!
//! ```no_run
//! use std::process::ExitCode;
//!
//! use anyhow::Result;
//! use cargo_plane_provider::{Endpoint, Host, Provider, run};
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! struct Config {
//!     host: String,
//! }
//!
//! struct Static;
//!
//! impl Provider for Static {
//!     type Config = Config;
//!
//!     fn up(&self, config: &Config, _host: &dyn Host) -> Result<Endpoint> {
//!         Ok(Endpoint::new(&config.host))
//!     }
//!
//!     fn down(&self, _config: &Config, _host: &dyn Host) -> Result<()> {
//!         Ok(())
//!     }
//!
//!     fn status(&self, config: &Config, _host: &dyn Host) -> Result<String> {
//!         Ok(format!("always-on host {}", config.host))
//!     }
//! }
//!
//! fn main() -> ExitCode {
//!     run(Static)
//! }
//! ```
//!
//! cargo-plane starts the plugin when it first needs the provider and talks to
//! it over a Unix socket (see [`protocol`]). The plugin keeps the terminal:
//! stdin, stdout and stderr are cargo-plane's, so commands it runs can prompt
//! as usual. Ask the user things through [`Host`] instead of reading stdin
//! yourself, so prompts look the same for every provider.

mod plugin;
pub mod protocol;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use plugin::run;
#[doc(hidden)]
pub use plugin::serve_on;

/// How to reach a running remote over SSH.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    /// Extra arguments for `ssh`, e.g. `["-o", "HostKeyAlias=..."]`.
    #[serde(default)]
    pub ssh_options: Vec<String>,
    /// Base directory for synced projects: absolute, or relative to the remote `$HOME`.
    #[serde(default)]
    pub remote_dir: Option<String>,
    /// Environment for remote cargo invocations.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Shell lines run before every remote command, e.g. to find the toolchain.
    #[serde(default)]
    pub prelude: Option<String>,
}

impl Endpoint {
    /// An endpoint with just a host (name, address or `~/.ssh/config` alias).
    pub fn new(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            ..Self::default()
        }
    }
}

/// A kind of remote machine: how to start, stop and describe one.
pub trait Provider {
    /// The remote's table from cargo-plane's config. It also holds the keys
    /// every remote has (`provider`, `env`, `ssh_options`, `remote_dir`), so
    /// don't deny unknown fields.
    type Config: DeserializeOwned;

    /// Make sure the remote is running and return how to reach it. May be
    /// slow (booting a machine); cargo-plane caches the result between runs.
    fn up(&self, config: &Self::Config, host: &dyn Host) -> Result<Endpoint>;

    /// Stop, suspend or destroy the remote.
    fn down(&self, config: &Self::Config, host: &dyn Host) -> Result<()>;

    /// One line on the remote's state.
    fn status(&self, config: &Self::Config, host: &dyn Host) -> Result<String>;

    /// Permanently delete everything the provider created, after asking.
    /// Returns `false` if the user backed out. cargo-plane then removes the
    /// remote from the config.
    fn purge(&self, config: &Self::Config, host: &dyn Host) -> Result<bool> {
        let _ = (config, host);
        bail!("this provider doesn't create anything, so there's nothing to purge")
    }

    /// How long SSH may take to come up after `up()` returns.
    fn boot_timeout(&self, config: &Self::Config) -> Duration {
        let _ = config;
        Duration::ZERO
    }

    /// `cargo plane setup <provider>`: create what the remote needs and save
    /// its config with [`Host::save_config`]. cargo-plane then connects and
    /// checks that cargo runs.
    fn setup(&self, setup: &Setup, host: &dyn Host) -> Result<()> {
        let _ = (setup, host);
        bail!("this provider has no setup; add the remote to the config yourself")
    }
}

/// What `cargo plane setup <provider>` was run with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Setup {
    /// Name of the remote being set up (`--remote`, default: the provider's name).
    pub remote: String,
    /// The frontend's project directory, if any.
    pub project_dir: Option<PathBuf>,
    /// The remote's current table in the project's config, if it exists.
    pub config: Option<Value>,
    /// Arguments after `setup <provider>`.
    pub args: Vec<String>,
}

impl Setup {
    /// Command used in setup instructions for the selected build environment.
    pub fn command(&self) -> &'static str {
        if self.docker() {
            "docker plane"
        } else {
            "cargo plane"
        }
    }

    /// Frontends can request Docker provisioning through the setup configuration.
    /// Missing settings retain the original Cargo environment and wire protocol.
    pub fn docker(&self) -> bool {
        self.config
            .as_ref()
            .and_then(|config| config.get("build_environment"))
            .and_then(Value::as_str)
            == Some("docker")
    }
}

/// Verify Docker access as the SSH user, without changing an existing daemon.
/// Docker frontends use Buildx to provision the BuildKit container afterwards.
pub fn require_docker(host: &dyn Host) -> Result<()> {
    let output = host.run_remote("docker info >/dev/null", true)?;
    if !output.success {
        bail!(
            "Docker is unavailable to the remote SSH user: {}\n\
             Install and start Docker, and give that user access to its socket. \
             For Codespaces, add the docker-in-docker devcontainer feature and rebuild; \
             then rerun setup. No Rust toolchain is needed.",
            output.stderr.trim()
        );
    }
    Ok(())
}

/// cargo-plane's side: the terminal, and during setup the config and the
/// remote itself.
pub trait Host {
    /// A cargo-style status line: `    Starting machine abc`.
    fn status(&self, verb: &str, message: &str);

    fn warn(&self, message: &str);

    /// Whether there's a terminal to ask questions on.
    fn interactive(&self) -> bool;

    /// Ask for a line of input; an empty answer picks `default`.
    fn ask(&self, question: &str, default: Option<&str>) -> Result<String>;

    fn confirm(&self, question: &str, default: bool) -> Result<bool>;

    /// Setup only: merge `config` (a JSON object) into the remote's table in
    /// the frontend's project configuration, creating it if needed.
    fn save_config(&self, config: &Value) -> Result<()>;

    /// Setup only, after [`Host::save_config`]: run a POSIX shell command on
    /// the remote, with the environment cargo would get. With `capture`
    /// false, its output goes to the terminal instead.
    fn run_remote(&self, command: &str, capture: bool) -> Result<RemoteOutput>;
}

/// What a command run with [`Host::run_remote`] did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteOutput {
    pub success: bool,
    /// Exit code; `None` if it was killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// What `up()` produced, with the provider's boot timeout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Started {
    pub endpoint: Endpoint,
    pub boot_timeout: Duration,
}

/// A [`Provider`] with its config still serialized: what cargo-plane works
/// with, whether the provider is built in or a plugin.
pub trait AnyProvider {
    fn up(&self, config: &Value, host: &dyn Host) -> Result<Started>;
    fn down(&self, config: &Value, host: &dyn Host) -> Result<()>;
    fn status(&self, config: &Value, host: &dyn Host) -> Result<String>;
    fn purge(&self, config: &Value, host: &dyn Host) -> Result<bool>;
    fn setup(&self, setup: &Setup, host: &dyn Host) -> Result<()>;
}

impl<P: Provider> AnyProvider for P {
    fn up(&self, config: &Value, host: &dyn Host) -> Result<Started> {
        let config = parse::<P::Config>(config)?;
        Ok(Started {
            endpoint: Provider::up(self, &config, host)?,
            boot_timeout: self.boot_timeout(&config),
        })
    }

    fn down(&self, config: &Value, host: &dyn Host) -> Result<()> {
        Provider::down(self, &parse(config)?, host)
    }

    fn status(&self, config: &Value, host: &dyn Host) -> Result<String> {
        Provider::status(self, &parse(config)?, host)
    }

    fn purge(&self, config: &Value, host: &dyn Host) -> Result<bool> {
        Provider::purge(self, &parse(config)?, host)
    }

    fn setup(&self, setup: &Setup, host: &dyn Host) -> Result<()> {
        Provider::setup(self, setup, host)
    }
}

fn parse<T: DeserializeOwned>(config: &Value) -> Result<T> {
    T::deserialize(config).context("invalid provider config")
}

/// `$XDG_CACHE_HOME/cargo-plane`, falling back to `~/.cache/cargo-plane`:
/// for pid files, logs and downloads.
pub fn cache_dir() -> Result<PathBuf> {
    xdg_dir("XDG_CACHE_HOME", ".cache")
}

/// `$XDG_DATA_HOME/cargo-plane`, falling back to `~/.local/share/cargo-plane`:
/// for tools a provider installs.
pub fn data_dir() -> Result<PathBuf> {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .context("$HOME is not set")
}

fn xdg_dir(var: &str, fallback: &str) -> Result<PathBuf> {
    let base = match std::env::var_os(var).filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home_dir()?.join(fallback),
    };
    Ok(base.join("cargo-plane"))
}

/// 64-bit FNV-1a: for names, ports and paths that must stay the same between
/// runs. Unlike `DefaultHasher`, its output never changes between Rust versions.
pub fn stable_hash(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable() {
        assert_eq!(stable_hash(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(stable_hash(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
