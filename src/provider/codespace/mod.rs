//! A GitHub Codespace. GitHub stops it after its idle timeout and only bills it
//! while it runs; `/workspaces` (where builds go) survives stops and rebuilds.
//! `cargo plane setup codespace` creates one.
//!
//! SSH goes through `gh codespace ssh --stdio` as a ProxyCommand, which starts
//! the codespace if needed, so it doesn't need a public address.

mod gh;
mod setup;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use self::gh::Gh;
use super::{Endpoint, Provider};
use crate::ui;

pub use setup::setup;

#[derive(Deserialize)]
pub struct Config {
    /// The codespace's name (not its display name), as `gh codespace list` shows it.
    codespace: String,
    /// Path to gh; found on PATH when unset.
    gh: Option<String>,
}

/// On the persistent disk, next to the repository checkout.
const REMOTE_DIR: &str = "/workspaces/.cargo-plane";

/// Devcontainer images (the `rust` feature, `devcontainers/rust`) install the
/// toolchain in /usr/local and point to it from their Docker ENV, which sshd
/// doesn't pass on. A rustup in `~/.cargo` (see `setup`) wins.
const PRELUDE: &str = r#"if [ ! -f "$HOME/.cargo/env" ] && [ -d /usr/local/cargo/bin ]; then
  export PATH="/usr/local/cargo/bin:$PATH"
  if [ -z "$RUSTUP_HOME" ] && [ -d /usr/local/rustup ]; then export RUSTUP_HOME=/usr/local/rustup; fi
  if [ -z "$CARGO_HOME" ] && [ -w /usr/local/cargo ]; then export CARGO_HOME=/usr/local/cargo; fi
fi
"#;

/// Options from gh's OpenSSH config that we set ourselves.
const OWN_OPTIONS: &[&str] = &[
    "host",
    "user",
    "loglevel",
    "controlmaster",
    "controlpath",
    "controlpersist",
];

pub struct Codespace(Config);

impl Codespace {
    pub fn new(config: Config) -> Self {
        Self(config)
    }

    fn gh(&self) -> Result<Gh> {
        Gh::require(self.0.gh.as_deref())
    }
}

impl Provider for Codespace {
    fn up(&self) -> Result<Endpoint> {
        let gh = self.gh()?;
        let name = &self.0.codespace;
        let codespace = gh.codespace(name)?;
        if codespace.state != "Available" {
            ui::status(
                "Starting",
                format!("codespace {name} ({})", codespace.state),
            );
        }
        // Starts the codespace and waits for it, then asks it for the SSH user.
        let config = gh
            .output(&["codespace", "ssh", "--config", "-c", name])
            .with_context(|| format!("can't get SSH settings for codespace `{name}`"))?;
        let (user, ssh_options) = parse_ssh_config(&config)?;

        Ok(Endpoint {
            host: format!("cs.{name}"),
            user: Some(user),
            port: None,
            ssh_options,
            remote_dir: Some(REMOTE_DIR.into()),
            env: Default::default(),
            prelude: Some(PRELUDE.into()),
        })
    }

    fn down(&self) -> Result<()> {
        let gh = self.gh()?;
        let name = &self.0.codespace;
        if !matches!(
            gh.codespace(name)?.state.as_str(),
            "Shutdown" | "ShuttingDown"
        ) {
            ui::status("Stopping", format!("codespace {name}"));
            gh.output(&["codespace", "stop", "-c", name])?;
        }
        Ok(())
    }

    fn status(&self) -> Result<String> {
        let cs = self.gh()?.codespace(&self.0.codespace)?;
        let label = if cs.display_name.is_empty() {
            cs.name
        } else {
            format!("{} ({})", cs.display_name, cs.name)
        };
        Ok(format!(
            "codespace {label} for {}, {}: {}",
            cs.repository, cs.machine_name, cs.state
        ))
    }

    fn purge(&self) -> Result<bool> {
        if !ui::interactive() {
            bail!("`cargo plane purge` asks for confirmation; run it in a terminal");
        }
        let gh = self.gh()?;
        let name = &self.0.codespace;
        if !gh.exists(name)? {
            ui::status(
                "Gone",
                format!("codespace `{name}` doesn't exist (anymore)"),
            );
            return Ok(true);
        }
        let cs = gh.codespace(name)?;
        eprintln!(
            "This permanently deletes the codespace `{name}` ({}, {}), including its build \
             caches and anything in it that isn't pushed.",
            cs.repository, cs.state
        );
        if ui::ask("Type the codespace name to confirm", None)? != *name {
            return Ok(false);
        }
        ui::status("Deleting", format!("codespace {name}"));
        // Without --force, gh asks again if there are unpushed changes.
        gh.interactive(&["codespace", "delete", "-c", name])?;
        Ok(true)
    }

    fn boot_timeout(&self) -> std::time::Duration {
        // `up()` waits for the codespace, but its sshd starts on first connect.
        std::time::Duration::from_secs(60)
    }
}

/// The user and `-o` options from `gh codespace ssh --config` output, which
/// looks like:
///
/// ```text
/// Host cs.<name>.<branch>
///     User codespace
///     ProxyCommand /usr/bin/gh cs ssh -c <name> --stdio -- -i ~/.ssh/codespaces.auto
///     UserKnownHostsFile=/dev/null
///     StrictHostKeyChecking no
///     ...
/// ```
fn parse_ssh_config(text: &str) -> Result<(String, Vec<String>)> {
    let mut user = None;
    let mut options = Vec::new();
    let mut hosts = 0;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.find(|c: char| c.is_whitespace() || c == '=') {
            Some(i) => (
                &line[..i],
                line[i + 1..].trim_start_matches(['=', ' ', '\t']),
            ),
            None => (line, ""),
        };
        let lower = key.to_ascii_lowercase();
        if lower == "host" {
            hosts += 1;
            // Only one block is expected with `-c`; ignore anything after it.
            if hosts > 1 {
                break;
            }
        } else if lower == "user" {
            user = Some(value.to_owned());
        } else if !OWN_OPTIONS.contains(&lower.as_str()) {
            options.extend(["-o".into(), format!("{key}={value}")]);
        }
    }
    let user = user.context("`gh codespace ssh --config` printed no SSH user")?;
    // gh's own config silences ssh entirely; we still want its errors.
    options.extend(["-o".into(), "LogLevel=ERROR".into()]);
    Ok((user, options))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gh_ssh_config() {
        let text = "Host cs.fluffy-x.main\n\
                    \tUser codespace\n\
                    \tProxyCommand /usr/bin/gh cs ssh -c fluffy-x --stdio -- -i /h/.ssh/codespaces.auto\n\
                    \tUserKnownHostsFile=/dev/null\n\
                    \tStrictHostKeyChecking no\n\
                    \tLogLevel quiet\n\
                    \tControlMaster auto\n\
                    \tIdentityFile /h/.ssh/codespaces.auto\n\n";
        let (user, options) = parse_ssh_config(text).unwrap();
        assert_eq!(user, "codespace");
        assert_eq!(
            options,
            [
                "-o",
                "ProxyCommand=/usr/bin/gh cs ssh -c fluffy-x --stdio -- -i /h/.ssh/codespaces.auto",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "IdentityFile=/h/.ssh/codespaces.auto",
                "-o",
                "LogLevel=ERROR",
            ]
        );
    }

    #[test]
    fn missing_user_is_an_error() {
        assert!(parse_ssh_config("").is_err());
    }
}
