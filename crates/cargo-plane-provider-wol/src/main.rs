//! A machine on your network that sleeps when you're not building: woken with
//! a Wake-on-LAN packet, suspended over SSH when you `cargo plane land`.
//!
//! ```toml
//! [remotes.desktop]
//! provider = "wol"
//! host = "desktop.lan"            # hostname or ~/.ssh/config alias
//! mac = "aa:bb:cc:dd:ee:ff"
//! # broadcast = "255.255.255.255:9"
//! # sleep_command = "sudo -n systemctl suspend"
//! # boot_timeout = 120            # seconds to wait for SSH after waking it
//! ```

use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cargo_plane_provider::{Endpoint, Host, Provider, Setup, run};
use serde::Deserialize;
use serde_json::{Value, json};

const SETUP_USAGE: &str = "\
Usage: cargo plane [--remote <name>] setup wol

Asks for the machine's SSH host and MAC address and adds the remote (default
name: wol) to the project's .cargo/plane.toml. Optional settings (broadcast,
sleep_command, boot_timeout) can be added there by hand.
";

#[derive(Deserialize)]
struct Config {
    host: String,
    mac: String,
    user: Option<String>,
    port: Option<u16>,
    #[serde(default = "default_broadcast")]
    broadcast: String,
    #[serde(default = "default_sleep_command")]
    sleep_command: String,
    #[serde(default = "default_boot_timeout")]
    boot_timeout: u64,
}

fn default_broadcast() -> String {
    "255.255.255.255:9".into()
}

fn default_sleep_command() -> String {
    "sudo -n systemctl suspend".into()
}

fn default_boot_timeout() -> u64 {
    120
}

impl Config {
    /// Where SSH would connect, following `~/.ssh/config` like ssh does.
    fn ssh_address(&self) -> (String, u16) {
        let mut cmd = Command::new("ssh");
        cmd.arg("-G");
        if let Some(port) = self.port {
            cmd.args(["-p", &port.to_string()]);
        }
        let resolved = cmd
            .arg(&self.host)
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        let field = |name: &str| {
            resolved.as_deref()?.lines().find_map(|line| {
                let (key, value) = line.split_once(' ')?;
                (key == name).then(|| value.to_owned())
            })
        };
        let host = field("hostname").unwrap_or_else(|| self.host.clone());
        let port = field("port")
            .and_then(|p| p.parse().ok())
            .or(self.port)
            .unwrap_or(22);
        (host, port)
    }

    fn awake(&self) -> bool {
        let (host, port) = self.ssh_address();
        (host.as_str(), port)
            .to_socket_addrs()
            .into_iter()
            .flatten()
            .any(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok())
    }
}

struct WakeOnLan;

impl Provider for WakeOnLan {
    type Config = Config;

    fn up(&self, config: &Config, host: &dyn Host) -> Result<Endpoint> {
        if !config.awake() {
            host.status("Waking", &format!("{} ({})", config.host, config.mac));
            wake(&config.mac, &config.broadcast)?;
        }
        Ok(Endpoint {
            user: config.user.clone(),
            port: config.port,
            ..Endpoint::new(&config.host)
        })
    }

    fn down(&self, config: &Config, host: &dyn Host) -> Result<()> {
        if !config.awake() {
            return Ok(());
        }
        host.status("Suspending", &config.host);
        let mut cmd = Command::new("ssh");
        cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
        if let Some(port) = config.port {
            cmd.args(["-p", &port.to_string()]);
        }
        if let Some(user) = &config.user {
            cmd.args(["-l", user]);
        }
        let status = cmd
            .arg(&config.host)
            .arg(&config.sleep_command)
            .stdin(Stdio::null())
            .status()
            .context("running ssh")?;
        // The connection often drops as the machine goes to sleep (255).
        if !status.success() && status.code() != Some(255) {
            bail!(
                "`{}` failed on {} ({status}); it needs to run without a password",
                config.sleep_command,
                config.host
            );
        }
        Ok(())
    }

    fn status(&self, config: &Config, _: &dyn Host) -> Result<String> {
        let state = if config.awake() {
            "awake"
        } else {
            "asleep or unreachable"
        };
        Ok(format!("{} ({}): {state}", config.host, config.mac))
    }

    fn boot_timeout(&self, config: &Config) -> Duration {
        Duration::from_secs(config.boot_timeout)
    }

    fn setup(&self, setup: &Setup, host: &dyn Host) -> Result<()> {
        match setup.args.as_slice() {
            [] => {}
            [help] if help == "-h" || help == "--help" => {
                print!("{SETUP_USAGE}");
                return Ok(());
            }
            [other, ..] => bail!("unknown option `{other}` for `setup wol`\n\n{SETUP_USAGE}"),
        }
        if !host.interactive() {
            bail!("`cargo plane setup wol` asks questions; run it in a terminal");
        }
        let current = |key: &str| {
            setup
                .config
                .as_ref()
                .and_then(|c| c.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let ssh_host = host.ask(
            "Machine to build on (hostname or ~/.ssh/config alias)",
            current("host").as_deref(),
        )?;
        let mac = loop {
            let mac = host.ask(
                "Its MAC address (on Linux: `ip link` on that machine)",
                current("mac").as_deref(),
            )?;
            match parse_mac(&mac) {
                Ok(_) => break mac,
                Err(err) => host.warn(&format!("{err:#}")),
            }
        };
        host.save_config(&json!({ "host": ssh_host, "mac": mac }))
    }
}

/// Send the magic packet: six 0xff bytes, then the MAC sixteen times.
fn wake(mac: &str, broadcast: &str) -> Result<()> {
    let mac = parse_mac(mac)?;
    let mut packet = vec![0xff; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&mac);
    }
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_broadcast(true)?;
    // UDP gets lost; a few copies don't hurt.
    for _ in 0..3 {
        socket
            .send_to(&packet, broadcast)
            .with_context(|| format!("sending the wake-up packet to {broadcast}"))?;
    }
    Ok(())
}

fn parse_mac(mac: &str) -> Result<[u8; 6]> {
    let bytes: Vec<u8> = mac
        .split([':', '-'])
        .map(|part| u8::from_str_radix(part, 16))
        .collect::<Result<_, _>>()
        .with_context(|| format!("`{mac}` isn't a MAC address like aa:bb:cc:dd:ee:ff"))?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("`{mac}` isn't a MAC address like aa:bb:cc:dd:ee:ff"))
}

fn main() -> ExitCode {
    run(WakeOnLan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_macs() {
        assert_eq!(
            parse_mac("AA:bb:0c:dd:ee:01").unwrap(),
            [0xaa, 0xbb, 0x0c, 0xdd, 0xee, 0x01]
        );
        assert!(parse_mac("aa-bb-cc-dd-ee-ff").is_ok());
        assert!(parse_mac("aa:bb:cc").is_err());
        assert!(parse_mac("zz:bb:cc:dd:ee:ff").is_err());
    }
}
