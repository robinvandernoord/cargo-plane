use std::path::PathBuf;

use anyhow::Result;
pub use cargo_plane_provider::{cache_dir, stable_hash as fnv1a};

/// `$XDG_CONFIG_HOME/cargo-plane`, falling back to `~/.config/cargo-plane`.
pub fn config_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => cargo_plane_provider::home_dir()?.join(".config"),
    };
    Ok(base.join("cargo-plane"))
}

/// Quote a string for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-./=:,+@%".contains(&b));
    if safe {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

pub fn shell_join<S: AsRef<str>>(args: impl IntoIterator<Item = S>) -> String {
    args.into_iter()
        .map(|a| shell_quote(a.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(shell_quote("--release"), "--release");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
    }
}
