//! A machine that's always on and reachable over SSH: a VPS, a desktop at home.

use anyhow::Result;
use serde::Deserialize;

use super::{Endpoint, Provider};

#[derive(Deserialize)]
pub struct Config {
    /// Hostname or `~/.ssh/config` alias.
    host: String,
    user: Option<String>,
    port: Option<u16>,
}

pub struct Ssh(Config);

impl Ssh {
    pub fn new(config: Config) -> Self {
        Self(config)
    }
}

impl Provider for Ssh {
    fn up(&self) -> Result<Endpoint> {
        Ok(Endpoint {
            host: self.0.host.clone(),
            user: self.0.user.clone(),
            port: self.0.port,
            ssh_options: Vec::new(),
            remote_dir: None,
            env: Default::default(),
            prelude: None,
        })
    }

    fn down(&self) -> Result<()> {
        Ok(())
    }

    fn status(&self) -> Result<String> {
        Ok(format!("static SSH host `{}`", self.0.host))
    }
}
