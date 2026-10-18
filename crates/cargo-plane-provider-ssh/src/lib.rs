//! A machine that's always on and reachable over SSH: a VPS, a desktop at home.

use anyhow::Result;
use cargo_plane_provider::{Endpoint, Host, Provider};
use serde::Deserialize;

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
