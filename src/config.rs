//! Config files: `.cargo/plane.toml` in the project (searched upwards from the
//! current directory), layered over `~/.config/cargo-plane/config.toml`.
//!
//! ```toml
//! remote = "dev"          # default remote
//!
//! [remotes.dev]
//! provider = "ssh"        # defaults to the remote's name
//! host = "my-box"
//! remote_dir = "builds"   # relative to the remote $HOME, or absolute
//! env = { RUST_LOG = "info" }
//! ssh_options = ["-o", "Compression=yes"]
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use toml::{Table, Value};

use crate::{ui, util};

pub const PROJECT_FILE: &str = ".cargo/plane.toml";

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    pub remote: Option<String>,
    #[serde(default)]
    pub remotes: BTreeMap<String, Table>,
}

/// Settings every remote supports, regardless of provider.
#[derive(Debug, Default, Deserialize)]
pub struct Common {
    pub remote_dir: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub ssh_options: Vec<String>,
}

pub struct RemoteConfig {
    pub name: String,
    pub provider: String,
    pub common: Common,
    pub table: Table,
}

impl Config {
    pub fn load(cwd: &Path) -> Result<Self> {
        let mut merged = Table::new();
        let global = util::config_dir()?.join("config.toml");
        for path in [Some(global), find_project_file(cwd)].into_iter().flatten() {
            if path.is_file() {
                merge(&mut merged, read(&path)?);
            }
        }
        Value::Table(merged)
            .try_into()
            .context("invalid cargo-plane config")
    }

    /// Pick a remote: explicit name, then `remote = ...`, then the only one defined.
    pub fn select(mut self, explicit: Option<&str>) -> Result<RemoteConfig> {
        let name = match explicit.map(str::to_owned).or(self.remote.take()) {
            Some(name) => name,
            None if self.remotes.len() == 1 => self.remotes.keys().next().unwrap().clone(),
            None if self.remotes.is_empty() => bail!(
                "no remotes configured\n\n\
                 Run `cargo plane setup <provider>` inside your Cargo project:\n\n  \
                 cargo plane setup local       # build on this machine\n  \
                 cargo plane setup ssh         # use an existing SSH host\n  \
                 cargo plane setup fly         # create a Fly.io machine\n  \
                 cargo plane setup codespace   # create or pick a GitHub Codespace\n\n\
                 Or add one to `{PROJECT_FILE}` or `~/.config/cargo-plane/config.toml`:\n\n  \
                 [remotes.dev]\n  provider = \"ssh\"\n  host = \"my-build-box\"\n"
            ),
            None => bail!(
                "multiple remotes configured ({}); pick one with `--remote <name>`, \
                 CARGO_PLANE_REMOTE or `remote = \"<name>\"` in the config",
                self.remotes.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
        };
        let Some(table) = self.remotes.remove(&name) else {
            let known: Vec<_> = self.remotes.keys().map(String::as_str).collect();
            let known = match known.as_slice() {
                [] => "no remotes are configured".to_owned(),
                names => format!("configured: {}", names.join(", ")),
            };
            bail!(
                "remote `{name}` is not configured ({known}); add it to `{PROJECT_FILE}` or \
                 ~/.config/cargo-plane/config.toml, or run `cargo plane setup <provider>` \
                 (local, ssh, fly or codespace); use `--remote {name}` before `setup` to name it"
            );
        };
        let provider = match table.get("provider") {
            None => name.clone(),
            Some(Value::String(p)) => p.clone(),
            Some(_) => bail!("remotes.{name}.provider must be a string"),
        };
        let common: Common = Value::Table(table.clone())
            .try_into()
            .with_context(|| format!("invalid config for remote `{name}`"))?;
        Ok(RemoteConfig {
            name,
            provider,
            common,
            table,
        })
    }
}

impl RemoteConfig {
    /// This remote's table as JSON, the way providers get it.
    pub fn json(&self) -> Result<serde_json::Value> {
        serde_json::to_value(&self.table)
            .with_context(|| format!("invalid config for remote `{}`", self.name))
    }
}

/// `[remotes.<name>]` in a project's config file, if it's there.
pub fn project_remote(path: &Path, name: &str) -> Result<Option<Table>> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(read(path)?
        .get("remotes")
        .and_then(|r| r.get(name))
        .and_then(Value::as_table)
        .cloned())
}

/// Replace (or add) `[remotes.<name>]` in a config file, keeping everything
/// else, comments included.
pub fn write_remote(path: &Path, name: &str, table: &Table) -> Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let section = section_text(name, table)?;
    let updated = with_section(&text, name, &section);
    if toml::from_str::<Table>(&updated).is_err() {
        bail!(
            "can't update remote `{name}` in {}; put this there yourself:\n\n{section}",
            path.display()
        );
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, updated).with_context(|| format!("writing {}", path.display()))
}

fn section_text(name: &str, table: &Table) -> Result<String> {
    let mut remotes = Table::new();
    remotes.insert(name.into(), Value::Table(table.clone()));
    let mut doc = Table::new();
    doc.insert("remotes".into(), Value::Table(remotes));
    Ok(toml::to_string(&doc)?)
}

/// `text` with `[remotes.<name>]` replaced by `section` where it was, or
/// with `section` appended.
fn with_section(text: &str, name: &str, section: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if let Some((start, end)) = section_range(&lines, name) {
        let mut out: Vec<&str> = lines[..start].to_vec();
        out.extend(section.lines());
        out.extend(&lines[end..]);
        return out.join("\n") + "\n";
    }
    let mut out = text.to_owned();
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out + section
}

/// Remove `[remotes.<name>]` (and its subtables) from the project's
/// `.cargo/plane.toml`, or else the global config, keeping everything else.
pub fn remove_remote(name: &str) -> Result<()> {
    let global = util::config_dir()?.join("config.toml");
    let candidates = [find_project_file(&std::env::current_dir()?), Some(global)];
    for path in candidates.into_iter().flatten() {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if let Some(updated) = remove_section(&text, name) {
            std::fs::write(&path, updated)?;
            ui::status(
                "Removed",
                format!("remote `{name}` from {}", path.display()),
            );
            return Ok(());
        }
    }
    ui::warn(format!(
        "remote `{name}` isn't in `{PROJECT_FILE}` or the global config; remove it yourself"
    ));
    Ok(())
}

/// Lines `start..end` holding `[remotes.<name>]` and its subtables, without
/// the blank lines after it.
fn section_range(lines: &[&str], name: &str) -> Option<(usize, usize)> {
    let header = format!("[remotes.{name}]");
    let sub = format!("[remotes.{name}.");
    let start = lines.iter().position(|l| l.trim() == header)?;
    let mut end = lines[start + 1..]
        .iter()
        .position(|l| {
            let l = l.trim_start();
            l.starts_with('[') && !l.starts_with(&sub)
        })
        .map_or(lines.len(), |i| start + 1 + i);
    while end > start + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    Some((start, end))
}

fn remove_section(text: &str, name: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let (start, end) = section_range(&lines, name)?;
    // Keep the blank lines before the next section; drop the one before ours instead.
    let start = if start > 0 && lines[start - 1].trim().is_empty() {
        start - 1
    } else {
        start
    };
    let mut kept: Vec<&str> = lines[..start].to_vec();
    kept.extend(&lines[end..]);
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Some(out)
}

fn find_project_file(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .map(|dir| dir.join(PROJECT_FILE))
        .find(|path| path.is_file())
}

fn read(path: &Path) -> Result<Table> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Deep-merge `over` into `base`; tables merge, everything else replaces.
fn merge(base: &mut Table, over: Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(Value::Table(base)), Value::Table(over)) => merge(base, over),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        Value::Table(toml::from_str(text).unwrap())
            .try_into()
            .unwrap()
    }

    #[test]
    fn merge_is_deep() {
        let mut base: Table =
            toml::from_str("remote = \"a\"\n[remotes.a]\nhost = \"x\"\nenv = { A = \"1\" }")
                .unwrap();
        let over: Table = toml::from_str("[remotes.a]\nenv = { B = \"2\" }").unwrap();
        merge(&mut base, over);
        let remote = &base["remotes"]["a"];
        assert_eq!(remote["host"].as_str(), Some("x"));
        assert_eq!(remote["env"]["A"].as_str(), Some("1"));
        assert_eq!(remote["env"]["B"].as_str(), Some("2"));
    }

    #[test]
    fn selects_only_remote_and_defaults_provider_to_name() {
        let remote = config("[remotes.ssh]\nhost = \"box\"")
            .select(None)
            .unwrap();
        assert_eq!(remote.name, "ssh");
        assert_eq!(remote.provider, "ssh");
    }

    #[test]
    fn explicit_remote_wins() {
        let text =
            "remote = \"a\"\n[remotes.a]\nprovider = \"ssh\"\n[remotes.b]\nprovider = \"fly\"";
        assert_eq!(config(text).select(None).unwrap().name, "a");
        assert_eq!(config(text).select(Some("b")).unwrap().provider, "fly");
        assert!(config(text).select(Some("c")).is_err());
    }

    #[test]
    fn removes_only_the_named_section() {
        let text = "remote = \"a\"\n\n[remotes.a]\nhost = \"x\"\n\n[remotes.fly]\napp = \"y\"\n[remotes.fly.env]\nA = \"1\"\n\n[remotes.b]\nhost = \"z\"\n";
        assert_eq!(
            remove_section(text, "fly").unwrap(),
            "remote = \"a\"\n\n[remotes.a]\nhost = \"x\"\n\n[remotes.b]\nhost = \"z\"\n"
        );
        assert_eq!(
            remove_section("[remotes.fly]\napp = \"y\"\n", "fly").unwrap(),
            ""
        );
        assert!(remove_section(text, "nope").is_none());
    }

    #[test]
    fn writes_remote_sections() {
        let mut table: Table = toml::from_str("provider = \"fly\"\napp = \"x\"").unwrap();
        let section = section_text("fly", &table).unwrap();
        assert_eq!(section, "[remotes.fly]\nprovider = \"fly\"\napp = \"x\"\n");

        // Replaces only its own section and keeps comments elsewhere.
        let text =
            "# mine\nremote = \"a\"\n\n[remotes.fly]\napp = \"old\"\n\n[remotes.a]\nhost = \"z\"\n";
        assert_eq!(
            with_section(text, "fly", &section),
            "# mine\nremote = \"a\"\n\n[remotes.fly]\nprovider = \"fly\"\napp = \"x\"\n\n[remotes.a]\nhost = \"z\"\n"
        );
        assert_eq!(with_section("", "fly", &section), section);
        assert_eq!(
            with_section("a = 1", "fly", &section),
            format!("a = 1\n\n{section}")
        );

        table.insert(
            "env".into(),
            toml::from_str::<Table>("A = \"1\"").unwrap().into(),
        );
        let nested = section_text("fly", &table).unwrap();
        assert!(nested.contains("[remotes.fly.env]"), "{nested}");
    }

    #[test]
    fn ambiguous_remote_errors() {
        let text = "[remotes.a]\n[remotes.b]";
        assert!(config(text).select(None).is_err());
    }
}
