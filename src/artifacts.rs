//! The parts of cargo's JSON messages we care about.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(tag = "reason")]
pub enum Message {
    #[serde(rename = "compiler-artifact")]
    Artifact(Artifact),
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct Artifact {
    pub package_id: String,
    pub manifest_path: PathBuf,
    pub target: Target,
    pub profile: Profile,
    pub filenames: Vec<PathBuf>,
    pub executable: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
pub struct Target {
    pub name: String,
    pub kind: Vec<String>,
    pub src_path: PathBuf,
}

#[derive(Debug, Deserialize)]
pub struct Profile {
    pub test: bool,
}

impl Artifact {
    pub fn has_kind(&self, kind: &str) -> bool {
        self.target.kind.iter().any(|k| k == kind)
    }

    /// Files worth copying back after `cargo build`: executables and
    /// libraries meant for consumers outside cargo. Rlibs stay remote.
    pub fn deliverables(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = self.executable.iter().cloned().collect();
        if ["cdylib", "dylib", "staticlib"]
            .iter()
            .any(|k| self.has_kind(k))
        {
            files.extend(self.filenames.iter().cloned());
        }
        files
    }

    pub fn package_dir(&self) -> &Path {
        self.manifest_path.parent().unwrap_or(Path::new("."))
    }

    /// Package name from a package id: `path+file:///x/foo#0.1.0`,
    /// `path+file:///x/dir#foo@0.1.0` or the legacy `foo 0.1.0 (path+...)`.
    pub fn package_name(&self) -> &str {
        let id = &self.package_id;
        if let Some((url, fragment)) = id.split_once('#') {
            match fragment.split_once('@') {
                Some((name, _)) => name,
                None => url.rsplit('/').next().unwrap_or(url),
            }
        } else {
            id.split(' ').next().unwrap_or(id)
        }
    }
}

pub fn parse_line(line: &str) -> Option<Message> {
    if !line.starts_with('{') {
        return None;
    }
    serde_json::from_str(line).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(package_id: &str) -> Artifact {
        Artifact {
            package_id: package_id.into(),
            manifest_path: "/p/Cargo.toml".into(),
            target: Target {
                name: "foo".into(),
                kind: vec!["bin".into()],
                src_path: "/p/src/main.rs".into(),
            },
            profile: Profile { test: false },
            filenames: vec!["/p/target/debug/foo".into()],
            executable: Some("/p/target/debug/foo".into()),
        }
    }

    #[test]
    fn package_names() {
        assert_eq!(artifact("path+file:///x/foo#0.1.0").package_name(), "foo");
        assert_eq!(
            artifact("path+file:///x/dir#foo@0.1.0").package_name(),
            "foo"
        );
        assert_eq!(artifact("foo 0.1.0 (path+file:///x)").package_name(), "foo");
    }

    #[test]
    fn parses_compiler_artifact() {
        let line = r#"{"reason":"compiler-artifact","package_id":"path+file:///p#0.1.0","manifest_path":"/p/Cargo.toml","target":{"kind":["bin"],"crate_types":["bin"],"name":"p","src_path":"/p/src/main.rs","edition":"2024","doc":true,"doctest":false,"test":true},"profile":{"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":false},"features":[],"filenames":["/p/target/debug/p"],"executable":"/p/target/debug/p","fresh":true}"#;
        let Some(Message::Artifact(a)) = parse_line(line) else {
            panic!()
        };
        assert_eq!(a.deliverables(), vec![PathBuf::from("/p/target/debug/p")]);
        assert!(matches!(
            parse_line(r#"{"reason":"build-finished","success":true}"#),
            Some(Message::Other)
        ));
        assert!(parse_line("warning: unused").is_none());
    }
}
