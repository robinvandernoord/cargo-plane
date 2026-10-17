//! Finding, installing and running `flyctl`.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::{ui, util};

/// Metadata marking the machine cargo-plane builds on.
pub const METADATA_KEY: &str = "cargo-plane";
pub const METADATA_VALUE: &str = "builder";

pub struct Flyctl {
    bin: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct Machine {
    pub id: String,
    pub state: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    config: MachineConfig,
}

#[derive(Debug, Default, Deserialize)]
struct MachineConfig {
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

impl Machine {
    pub fn is_builder(&self) -> bool {
        self.config.metadata.get(METADATA_KEY).map(String::as_str) == Some(METADATA_VALUE)
    }
}

impl Flyctl {
    /// Configured path, then `fly`/`flyctl` on PATH, then our own install.
    pub fn locate(configured: Option<&str>) -> Option<Self> {
        if let Some(bin) = configured {
            return Some(Self { bin: bin.into() });
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        let on_path = std::env::split_paths(&path)
            .flat_map(|dir| [dir.join("fly"), dir.join("flyctl")])
            .find(|bin| bin.is_file());
        let installed = install_dir()
            .ok()
            .map(|dir| dir.join("flyctl"))
            .filter(|bin| bin.is_file());
        on_path.or(installed).map(|bin| Self { bin })
    }

    pub fn require(configured: Option<&str>) -> Result<Self> {
        Self::locate(configured)
            .context("flyctl not found; run `cargo plane setup fly` to install it")
    }

    pub fn path(&self) -> &Path {
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
            bail!("`flyctl {}` failed ({})", join(args), output.status);
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub fn json<T: DeserializeOwned, S: AsRef<str>>(&self, args: &[S]) -> Result<T> {
        let out = self.output(args)?;
        serde_json::from_str(&out)
            .with_context(|| format!("parsing output of `flyctl {}`", join(args)))
    }

    /// Run attached to the terminal (prompts, browser login, build output).
    pub fn interactive<S: AsRef<str>>(&self, args: &[S]) -> Result<()> {
        let status = self
            .command(args)
            .status()
            .with_context(|| format!("running {}", self.bin.display()))?;
        if !status.success() {
            bail!("`flyctl {}` failed ({status})", join(args));
        }
        Ok(())
    }

    pub fn logged_in(&self) -> bool {
        self.command(&["auth", "whoami"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    pub fn app_exists(&self, app: &str) -> bool {
        self.command(&["machine", "list", "--app", app, "--json"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    pub fn machines(&self, app: &str) -> Result<Vec<Machine>> {
        self.json(&["machine", "list", "--app", app, "--json"])
    }

    /// The builder machine: the configured ID, or the one tagged by `setup`.
    pub fn builder(&self, app: &str, id: Option<&str>) -> Result<Option<Machine>> {
        let machines = self.machines(app)?;
        Ok(machines.into_iter().find(|m| match id {
            Some(id) => m.id == id,
            None => m.is_builder(),
        }))
    }
}

fn join<S: AsRef<str>>(args: &[S]) -> String {
    args.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(" ")
}

/// `$XDG_DATA_HOME/cargo-plane/bin`, falling back to `~/.local/share/cargo-plane/bin`.
pub fn install_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => util::home_dir()?.join(".local/share"),
    };
    Ok(base.join("cargo-plane").join("bin"))
}

/// Download the latest flyctl release from GitHub, verify it against the
/// release's checksums and install it to our own bin dir.
pub fn install() -> Result<Flyctl> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "Linux_x86_64",
        ("linux", "aarch64") => "Linux_arm64",
        ("macos", "x86_64") => "macOS_x86_64",
        ("macos", "aarch64") => "macOS_arm64",
        (os, arch) => {
            bail!("no flyctl release for {os}/{arch}; install it yourself and put it on PATH")
        }
    };

    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
    }
    let release: Release = serde_json::from_slice(&curl(&[
        "https://api.github.com/repos/superfly/flyctl/releases/latest",
    ])?)
    .context("parsing GitHub release info")?;
    let version = release.tag_name.trim_start_matches('v');
    let base = format!(
        "https://github.com/superfly/flyctl/releases/download/{}",
        release.tag_name
    );
    let archive = format!("flyctl_{version}_{platform}.tar.gz");

    let work = util::cache_dir()?.join("flyctl-download");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;

    ui::status("Downloading", format!("flyctl {version} ({platform})"));
    let checksums = String::from_utf8(curl(&[&format!("{base}/flyctl_{version}_checksums.txt")])?)?;
    let expected = checksums
        .lines()
        .find_map(|line| {
            let (hash, name) = line.split_once(char::is_whitespace)?;
            (name.trim() == archive).then(|| hash.to_owned())
        })
        .with_context(|| format!("{archive} missing from the release checksums"))?;
    let archive_path = work.join(&archive);
    curl(&[
        "--output",
        archive_path.to_str().unwrap(),
        &format!("{base}/{archive}"),
    ])?;

    let actual = sha256(&archive_path)?;
    if actual != expected {
        bail!("checksum mismatch for {archive}: expected {expected}, got {actual}");
    }
    ui::status("Verified", "sha256 checksum");

    let status = Command::new("tar")
        .args(["-xzf", &archive, "flyctl"])
        .current_dir(&work)
        .status()
        .context("running tar")?;
    if !status.success() {
        bail!("extracting {archive} failed");
    }

    let dir = install_dir()?;
    fs::create_dir_all(&dir)?;
    let bin = dir.join("flyctl");
    fs::rename(work.join("flyctl"), &bin)
        .or_else(|_| fs::copy(work.join("flyctl"), &bin).map(drop))?;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))?;
    let _ = fs::remove_dir_all(&work);
    ui::status("Installed", bin.display());
    Ok(Flyctl { bin })
}

fn curl(args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location"])
        .args(args)
        .stderr(Stdio::inherit())
        .output()
        .context("running curl")?;
    if !output.status.success() {
        bail!("download failed: {}", args.last().unwrap_or(&""));
    }
    Ok(output.stdout)
}

fn sha256(path: &Path) -> Result<String> {
    let attempts: [(&str, &[&str]); 2] = [("sha256sum", &[]), ("shasum", &["-a", "256"])];
    for (tool, args) in attempts {
        if let Ok(output) = Command::new(tool).args(args).arg(path).output()
            && output.status.success()
        {
            let out = String::from_utf8_lossy(&output.stdout);
            return out
                .split_whitespace()
                .next()
                .map(str::to_owned)
                .context("empty checksum output");
        }
    }
    bail!("neither sha256sum nor shasum is available to verify the download")
}
