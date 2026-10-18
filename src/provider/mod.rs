//! Providers manage a remote machine's lifecycle and say how to reach it.
//! Everything else (sync, running cargo, fetching artifacts) is shared.

mod codespace;
mod fly;
mod ssh;

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::RemoteConfig;

pub use codespace::setup as setup_codespace;
pub use fly::setup as setup_fly;

/// How to reach a running remote over SSH.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

pub trait Provider {
    /// Make sure the remote is running and return how to reach it.
    /// May be slow (booting a machine); the result is cached between runs.
    fn up(&self) -> Result<Endpoint>;

    /// Stop, suspend or destroy the remote.
    fn down(&self) -> Result<()>;

    /// Human-readable state of the remote.
    fn status(&self) -> Result<String>;

    /// Permanently delete everything the provider created, after asking.
    /// Returns `false` if the user backed out.
    fn purge(&self) -> Result<bool> {
        bail!("this provider doesn't create anything, so there's nothing to purge")
    }

    /// How long SSH may take to come up after `up()` returns.
    fn boot_timeout(&self) -> Duration {
        Duration::ZERO
    }
}

pub fn from_config(remote: &RemoteConfig) -> Result<Box<dyn Provider>> {
    Ok(match remote.provider.as_str() {
        "ssh" => Box::new(ssh::Ssh::new(remote.provider_config()?)),
        "fly" => Box::new(fly::Fly::new(remote.provider_config()?)),
        "codespace" => Box::new(codespace::Codespace::new(remote.provider_config()?)),
        other => bail!(
            "unknown provider `{other}` for remote `{}` (available: ssh, fly, codespace)",
            remote.name
        ),
    })
}
