use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::process::Stdio;

use anyhow::{Result, bail};
use cargo_plane_provider::{Host, RemoteOutput};
use serde_json::Value;

use crate::builder::{self, Connection};
use crate::config::Config;

pub struct Terminal;

impl Host for Terminal {
    fn status(&self, verb: &str, message: &str) {
        eprintln!("{verb:>12} {message}");
    }

    fn warn(&self, message: &str) {
        eprintln!("warning: {message}");
    }

    fn interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }

    fn ask(&self, question: &str, default: Option<&str>) -> Result<String> {
        if !self.interactive() {
            bail!("{question}: run this command in a terminal");
        }
        loop {
            match default {
                Some(default) if !default.is_empty() => eprint!("{question} [{default}]: "),
                _ => eprint!("{question}: "),
            }
            std::io::stderr().flush()?;
            let mut answer = String::new();
            if std::io::stdin().read_line(&mut answer)? == 0 {
                bail!("aborted");
            }
            if !answer.trim().is_empty() {
                return Ok(answer.trim().into());
            }
            if let Some(default) = default {
                return Ok(default.into());
            }
        }
    }

    fn confirm(&self, question: &str, default: bool) -> Result<bool> {
        let hint = if default { "Y/n" } else { "y/N" };
        loop {
            let answer = self.ask(&format!("{question} [{hint}]"), Some(""))?;
            match answer.to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => {}
            }
        }
    }

    fn save_config(&self, _: &Value) -> Result<()> {
        bail!("configuration can only be saved during setup")
    }
    fn run_remote(&self, _: &str, _: bool) -> Result<RemoteOutput> {
        bail!("remote commands are only available during setup")
    }
}

pub struct SetupHost<'a> {
    pub config: &'a Config,
    pub remote: &'a str,
    pub provider: &'a str,
    globals: &'a [OsString],
    pub saved: Cell<bool>,
    connection: RefCell<Option<Connection>>,
}

impl<'a> SetupHost<'a> {
    pub fn new(
        config: &'a Config,
        remote: &'a str,
        provider: &'a str,
        globals: &'a [OsString],
    ) -> Self {
        Self {
            config,
            remote,
            provider,
            globals,
            saved: Cell::new(false),
            connection: RefCell::new(None),
        }
    }

    pub fn finish(&self) -> Result<()> {
        if !self.saved.get() || self.provider == "local" {
            return Ok(());
        }
        // The provider saves configuration before provisioning. Reload it rather
        // than reusing the snapshot from the beginning of setup.
        self.connect()?;
        let connection = self.connection.borrow();
        connection.as_ref().unwrap().ensure_builder(self.globals)?;
        self.status(
            "Ready",
            &format!(
                "try `docker plane --remote {} build --load -t my-app .`",
                self.remote
            ),
        );
        Ok(())
    }

    fn connect(&self) -> Result<()> {
        if self.connection.borrow().is_none() {
            let remote = Config::load()?.select(Some(self.remote))?;
            let connection = builder::connect(&remote)?;
            *self.connection.borrow_mut() = Some(connection);
        }
        Ok(())
    }
}

impl Host for SetupHost<'_> {
    fn status(&self, verb: &str, message: &str) {
        Terminal.status(verb, message);
    }
    fn warn(&self, message: &str) {
        Terminal.warn(message);
    }
    fn interactive(&self) -> bool {
        Terminal.interactive()
    }
    fn ask(&self, question: &str, default: Option<&str>) -> Result<String> {
        Terminal.ask(question, default)
    }
    fn confirm(&self, question: &str, default: bool) -> Result<bool> {
        Terminal.confirm(question, default)
    }

    fn save_config(&self, config: &Value) -> Result<()> {
        self.config.save(self.remote, self.provider, config)?;
        self.saved.set(true);
        Ok(())
    }

    fn run_remote(&self, command: &str, capture: bool) -> Result<RemoteOutput> {
        self.connect()?;
        let connection = self.connection.borrow();
        let mut command = connection.as_ref().unwrap().ssh_command(command);
        command.stdin(Stdio::null());
        if capture {
            let output = command.output()?;
            Ok(RemoteOutput {
                success: output.status.success(),
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into(),
                stderr: String::from_utf8_lossy(&output.stderr).into(),
            })
        } else {
            let status = command.status()?;
            Ok(RemoteOutput {
                success: status.success(),
                code: status.code(),
                ..RemoteOutput::default()
            })
        }
    }
}
