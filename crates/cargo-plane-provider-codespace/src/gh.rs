//! Finding and running the GitHub CLI.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

pub struct Gh {
    bin: PathBuf,
}

/// Fields of `gh codespace view --json` / `list --json` we use.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Codespace {
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    pub state: String,
    #[serde(default)]
    pub repository: String,
    #[serde(default)]
    pub machine_name: String,
}

impl Gh {
    /// Configured path, then `gh` on PATH.
    pub fn locate(configured: Option<&str>) -> Option<Self> {
        if let Some(bin) = configured {
            return Some(Self { bin: bin.into() });
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path)
            .map(|dir| dir.join("gh"))
            .find(|bin| bin.is_file())
            .map(|bin| Self { bin })
    }

    pub fn require(configured: Option<&str>) -> Result<Self> {
        Self::locate(configured)
            .context("the GitHub CLI (`gh`) is not installed; see https://cli.github.com")
    }

    pub fn path(&self) -> &std::path::Path {
        &self.bin
    }

    pub fn command<S: AsRef<str>>(&self, args: &[S]) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args.iter().map(AsRef::as_ref));
        cmd
    }

    /// Run with stderr shown, returning stdout.
    pub fn output<S: AsRef<str>>(&self, args: &[S]) -> Result<String> {
        let output = self
            .command(args)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .with_context(|| format!("running {}", self.bin.display()))?;
        if !output.status.success() {
            bail!("`gh {}` failed ({})", join(args), output.status);
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub fn json<T: DeserializeOwned, S: AsRef<str>>(&self, args: &[S]) -> Result<T> {
        let out = self.output(args)?;
        serde_json::from_str(&out).with_context(|| format!("parsing output of `gh {}`", join(args)))
    }

    /// Run attached to the terminal (prompts, browser login).
    pub fn interactive<S: AsRef<str>>(&self, args: &[S]) -> Result<()> {
        let status = self
            .command(args)
            .status()
            .with_context(|| format!("running {}", self.bin.display()))?;
        if !status.success() {
            bail!("`gh {}` failed ({status})", join(args));
        }
        Ok(())
    }

    /// Succeeds quietly, or not at all.
    pub fn succeeds<S: AsRef<str>>(&self, args: &[S]) -> bool {
        self.command(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    pub fn logged_in(&self) -> bool {
        self.succeeds(&["auth", "status", "--hostname", "github.com"])
    }

    /// Codespace commands need the `codespace` token scope, which
    /// `gh auth login` doesn't ask for by default.
    pub fn has_codespace_scope(&self) -> bool {
        self.succeeds(&["api", "user/codespaces?per_page=1", "--silent"])
    }

    pub fn codespace(&self, name: &str) -> Result<Codespace> {
        self.json(&["codespace", "view", "-c", name, "--json", CODESPACE_FIELDS])
            .with_context(|| {
                format!(
                    "can't find codespace `{name}` (stopped codespaces are deleted after their \
                     retention period); run `cargo plane setup codespace` to create a new one"
                )
            })
    }

    /// Whether the codespace exists; an error if gh can't tell.
    pub fn exists(&self, name: &str) -> Result<bool> {
        let all: Vec<Codespace> = self.json(&[
            "codespace",
            "list",
            "--limit",
            "1000",
            "--json",
            CODESPACE_FIELDS,
        ])?;
        Ok(all.iter().any(|c| c.name == name))
    }

    pub fn codespaces(&self, repo: &str) -> Result<Vec<Codespace>> {
        self.json(&[
            "codespace",
            "list",
            "--repo",
            repo,
            "--json",
            CODESPACE_FIELDS,
        ])
    }
}

const CODESPACE_FIELDS: &str = "name,displayName,state,repository,machineName";

fn join<S: AsRef<str>>(args: &[S]) -> String {
    args.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(" ")
}
