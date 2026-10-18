//! cargo-plane's side of `cargo_plane_provider::Host`: what providers may ask
//! of us. `UiHost` for ordinary commands, `SetupHost` for `cargo plane setup`.

use std::cell::{Cell, RefCell, RefMut};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use cargo_plane_provider::{Host, RemoteOutput};
use serde_json::Value;

use crate::config::{self, Config, PROJECT_FILE};
use crate::remote::{Conn, Remote};
use crate::ui;

/// Add the project's plane.toml to the `.gitignore` next to it: what `setup`
/// creates belongs to whoever ran it. `.cargo/config.toml` stays tracked.
fn ignore_in_git(path: &Path) -> Result<()> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return Ok(());
    };
    let gitignore = dir.join(".gitignore");
    let mut text = std::fs::read_to_string(&gitignore).unwrap_or_default();
    if text
        .lines()
        .any(|line| line.trim().trim_start_matches('/') == name)
    {
        return Ok(());
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text += &format!("{name}\n");
    std::fs::write(&gitignore, text).with_context(|| format!("writing {}", gitignore.display()))?;
    ui::status(
        "Ignored",
        format!(
            "{name} in {} (remove it there to share the remote with your team)",
            gitignore.display()
        ),
    );
    Ok(())
}

/// The terminal, and nothing else.
pub struct UiHost;

impl Host for UiHost {
    fn status(&self, verb: &str, message: &str) {
        ui::status(verb, message);
    }

    fn warn(&self, message: &str) {
        ui::warn(message);
    }

    fn interactive(&self) -> bool {
        ui::interactive()
    }

    fn ask(&self, question: &str, default: Option<&str>) -> Result<String> {
        ui::ask(question, default)
    }

    fn confirm(&self, question: &str, default: bool) -> Result<bool> {
        ui::confirm(question, default)
    }

    fn save_config(&self, _: &Value) -> Result<()> {
        bail!("providers can only save their config during `cargo plane setup`")
    }

    fn run_remote(&self, _: &str, _: bool) -> Result<RemoteOutput> {
        bail!("providers can only run remote commands during `cargo plane setup`")
    }
}

/// The terminal, the project's config and, once configured, the remote.
pub struct SetupHost {
    remote: String,
    provider: String,
    project_dir: Option<PathBuf>,
    saved: Cell<bool>,
    conn: RefCell<Option<Conn>>,
}

impl SetupHost {
    pub fn new(remote: &str, provider: &str, project_dir: Option<PathBuf>) -> Self {
        Self {
            remote: remote.into(),
            provider: provider.into(),
            project_dir,
            saved: Cell::new(false),
            conn: RefCell::new(None),
        }
    }

    fn connect(&self) -> Result<RefMut<'_, Conn>> {
        let mut conn = self.conn.borrow_mut();
        if conn.is_none() {
            let dir = match &self.project_dir {
                Some(dir) => dir.clone(),
                None => std::env::current_dir()?,
            };
            let remote = Remote::new(Config::load(&dir)?.select(Some(&self.remote))?)?;
            *conn = Some(remote.takeoff()?);
        }
        Ok(RefMut::map(conn, |c| c.as_mut().unwrap()))
    }

    /// If the provider saved a config, check that cargo runs on the remote.
    pub fn finish(self) -> Result<()> {
        if !self.saved.get() {
            return Ok(());
        }
        let name = &self.remote;
        if self.provider == "local" {
            ui::status(
                "Ready",
                format!("local execution; try `cargo plane --remote {name} check`"),
            );
            return Ok(());
        }
        let output = self
            .connect()?
            .shell("cargo --version")?
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            bail!(
                "connected, but `cargo --version` failed on remote `{name}` ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        ui::status(
            "Ready",
            format!(
                "{} on remote `{name}`; try `cargo plane --remote {name} check`",
                String::from_utf8_lossy(&output.stdout).trim()
            ),
        );
        Ok(())
    }
}

impl Host for SetupHost {
    fn status(&self, verb: &str, message: &str) {
        UiHost.status(verb, message);
    }

    fn warn(&self, message: &str) {
        UiHost.warn(message);
    }

    fn interactive(&self) -> bool {
        UiHost.interactive()
    }

    fn ask(&self, question: &str, default: Option<&str>) -> Result<String> {
        UiHost.ask(question, default)
    }

    fn confirm(&self, question: &str, default: bool) -> Result<bool> {
        UiHost.confirm(question, default)
    }

    fn save_config(&self, config: &Value) -> Result<()> {
        let dir = self
            .project_dir
            .as_ref()
            .context("run `cargo plane setup` inside the Cargo project the remote is for")?;
        let path = dir.join(PROJECT_FILE);
        let update = match toml::Value::try_from(config) {
            Ok(toml::Value::Table(table)) => table,
            Ok(_) => bail!("a provider's config must be a table"),
            Err(err) => return Err(err).context("the provider's config can't be written as TOML"),
        };

        let existing = config::project_remote(&path, &self.remote)?;
        let mut merged = existing.clone().unwrap_or_default();
        merged.insert(
            "provider".into(),
            toml::Value::String(self.provider.clone()),
        );
        merged.extend(update);

        if existing.as_ref() == Some(&merged) {
            ui::status(
                "Configured",
                format!("remote `{}` in {} (unchanged)", self.remote, path.display()),
            );
        } else {
            let created = !path.exists();
            config::write_remote(&path, &self.remote, &merged)?;
            ui::status(
                "Configured",
                format!("remote `{}` in {}", self.remote, path.display()),
            );
            if created {
                ignore_in_git(&path)?;
            }
        }
        self.saved.set(true);
        // Reconnect with the new config next time.
        *self.conn.borrow_mut() = None;
        Ok(())
    }

    fn run_remote(&self, command: &str, capture: bool) -> Result<RemoteOutput> {
        if !self.saved.get() {
            bail!("a provider can only run remote commands after saving its config");
        }
        let mut cmd = self.connect()?.shell(command)?;
        cmd.stdin(Stdio::null());
        if capture {
            let output = cmd.output()?;
            Ok(RemoteOutput {
                success: output.status.success(),
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        } else {
            let status = cmd.status()?;
            Ok(RemoteOutput {
                success: status.success(),
                code: status.code(),
                ..RemoteOutput::default()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_plane_toml_once() {
        let dir = std::env::temp_dir().join(format!("cargo-plane-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let gitignore = dir.join(".gitignore");
        std::fs::write(&gitignore, "target").unwrap();
        let plane = dir.join("plane.toml");
        ignore_in_git(&plane).unwrap();
        ignore_in_git(&plane).unwrap();
        assert_eq!(
            std::fs::read_to_string(&gitignore).unwrap(),
            "target\nplane.toml\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
