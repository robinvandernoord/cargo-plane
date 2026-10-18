//! Keep Rust in the project's dev container configuration across rebuilds.

use std::fs;
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};
use cargo_plane_provider::Host;
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{CstInputValue, CstRootNode};

const RUST_FEATURE: &str = "ghcr.io/devcontainers/features/rust";
const DEFAULT_CONFIG: &str = ".devcontainer/devcontainer.json";
const DEFAULT_CONTENT: &str =
    "{\n  \"image\": \"mcr.microsoft.com/devcontainers/universal:2\"\n}\n";

pub fn ensure_rust(root: &Path, configured: Option<&str>, host: &dyn Host) -> Result<()> {
    let relative = if let Some(path) = configured {
        let path = Path::new(path);
        if path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
            bail!("--devcontainer-path must be a path inside the project");
        }
        path.to_owned()
    } else if root.join(DEFAULT_CONFIG).exists() {
        DEFAULT_CONFIG.into()
    } else if root.join(".devcontainer.json").exists() {
        ".devcontainer.json".into()
    } else {
        let dir = root.join(".devcontainer");
        if dir.is_dir() {
            for entry in fs::read_dir(&dir).context("reading .devcontainer")? {
                if entry?.path().join("devcontainer.json").is_file() {
                    bail!(
                        "the project has alternative dev containers; select one with \
                         --devcontainer-path to add the Rust feature"
                    );
                }
            }
        }
        DEFAULT_CONFIG.into()
    };
    let path = root.join(&relative);
    let original = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && configured.is_none() => None,
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let document = CstRootNode::parse(
        original.as_deref().unwrap_or(DEFAULT_CONTENT),
        &ParseOptions::default(),
    )
    .with_context(|| format!("parsing {}", path.display()))?;
    let object = document
        .object_value()
        .with_context(|| format!("{} must contain a JSON object", path.display()))?;
    let features = object
        .object_value_or_create("features")
        .with_context(|| format!("features in {} must be an object", path.display()))?;
    if features.properties().iter().any(|property| {
        property.decoded_name().is_some_and(|name| {
            name.strip_prefix(RUST_FEATURE)
                .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with([':', '@']))
        })
    }) {
        return Ok(());
    }
    features.append(
        &format!("{RUST_FEATURE}:1"),
        CstInputValue::Object(Vec::new()),
    );
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&path, document.to_string())
        .with_context(|| format!("writing {}", path.display()))?;
    host.status(
        "Updated",
        &format!("{} with the Rust feature", relative.display()),
    );
    host.warn(&format!(
        "commit and push {} to the codespace's repository branch, then rebuild the \
         codespace to use the Rust feature; setup can install Rust for the current container",
        relative.display()
    ));
    Ok(())
}
