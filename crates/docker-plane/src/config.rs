use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use toml_edit::{DocumentMut, Item, Table};

pub const PROJECT_FILE: &str = ".docker/plane.toml";

pub struct Config {
    pub project: PathBuf,
    global: PathBuf,
    data: Value,
}

pub struct Remote {
    pub name: String,
    pub provider: String,
    pub config: Value,
}

impl Config {
    pub fn load() -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let project = cwd
            .ancestors()
            .map(|dir| dir.join(PROJECT_FILE))
            .find(|path| path.is_file())
            .unwrap_or_else(|| cwd.join(PROJECT_FILE));
        let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            Some(base) => PathBuf::from(base),
            None => cargo_plane_provider::home_dir()?.join(".config"),
        };
        let global = base.join("docker-plane/config.toml");
        let mut data = serde_json::json!({});
        for path in [&global, &project] {
            if path.exists() {
                let text = fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let table: toml::Table =
                    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                merge(&mut data, serde_json::to_value(table)?);
            }
        }
        if data
            .get("remotes")
            .is_some_and(|remotes| !remotes.is_object())
        {
            bail!("remotes must be a table in docker-plane configuration");
        }
        Ok(Self {
            project,
            global,
            data,
        })
    }

    pub fn root(&self) -> &Path {
        self.project.parent().unwrap().parent().unwrap()
    }

    pub fn existing(&self, name: &str) -> Option<Value> {
        self.data.get("remotes")?.get(name).cloned()
    }

    pub fn select(&self, explicit: Option<&str>) -> Result<Remote> {
        let remotes = self.data.get("remotes").and_then(Value::as_object);
        let configured = self.data.get("remote");
        if configured.is_some_and(|value| !value.is_string()) {
            bail!("remote must be a string in docker-plane configuration");
        }
        let name = explicit.or_else(|| configured.and_then(Value::as_str));
        let name = match name {
            Some(name) => name.to_owned(),
            None => match remotes {
                Some(remotes) if remotes.len() == 1 => remotes.keys().next().unwrap().clone(),
                Some(remotes) if !remotes.is_empty() => bail!(
                    "multiple Docker remotes configured; select one with --remote <name>, \
                     DOCKER_PLANE_REMOTE or remote = \"<name>\""
                ),
                _ => bail!(
                    "no Docker remotes configured; run `docker plane setup <local|ssh|fly|codespace>`"
                ),
            },
        };
        validate_name(&name)?;
        let mut config = self.existing(&name).with_context(|| {
            format!(
                "remote `{name}` is not configured in {PROJECT_FILE} or {}",
                self.global.display()
            )
        })?;
        if !config.is_object() {
            bail!("remotes.{name} must be a table");
        }
        let provider = match config.get("provider") {
            None => name.clone(),
            Some(Value::String(provider)) => provider.clone(),
            Some(_) => bail!("remotes.{name}.provider must be a string"),
        };
        config["build_environment"] = "docker".into();
        Ok(Remote {
            name,
            provider,
            config,
        })
    }

    /// Update just the remote table, preserving comments and unrelated settings.
    pub fn save(&self, name: &str, provider: &str, update: &Value) -> Result<()> {
        validate_name(name)?;
        let mut value = self.existing(name).unwrap_or_else(|| serde_json::json!({}));
        merge(&mut value, update.clone());
        value["provider"] = provider.into();
        value["build_environment"] = "docker".into();
        let body = toml::to_string(&value)?;
        let body: DocumentMut = body.parse()?;
        let mut doc = read_document(&self.project)?;
        if doc.get("remotes").is_none() {
            let mut remotes = Table::new();
            remotes.set_implicit(true);
            doc["remotes"] = Item::Table(remotes);
        }
        let remotes = doc["remotes"]
            .as_table_like_mut()
            .context("remotes must be a table")?;
        if let Some(Item::Table(existing)) = remotes.get_mut(name) {
            merge_table(existing, body.as_table());
        } else {
            remotes.insert(name, Item::Table(body.as_table().clone()));
        }
        write_document(&self.project, &doc)?;
        let ignore = self.project.parent().unwrap().join(".gitignore");
        let mut text = match fs::read_to_string(&ignore) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err.into()),
        };
        if !text
            .lines()
            .any(|line| matches!(line.trim(), "plane.toml" | "/plane.toml"))
        {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("plane.toml\n");
            fs::write(ignore, text)?;
        }
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        // Remove the nearest definition; a global remote with the same name can
        // describe a different machine and must remain untouched.
        for path in [&self.project, &self.global] {
            let mut doc = read_document(path)?;
            let removed = doc
                .get_mut("remotes")
                .and_then(Item::as_table_like_mut)
                .and_then(|remotes| remotes.remove(name))
                .is_some();
            if removed {
                if doc.get("remote").and_then(Item::as_str) == Some(name) {
                    doc.remove("remote");
                }
                write_document(path, &doc)?;
                if path == &self.project {
                    let global = read_document(&self.global)?;
                    if global.get("remotes").and_then(|r| r.get(name)).is_some() {
                        eprintln!(
                            "warning: global remote `{name}` remains in {}; check it before reusing that name",
                            self.global.display()
                        );
                    }
                }
                break;
            }
        }
        Ok(())
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        bail!("remote names must contain only letters, digits, underscores and hyphens");
    }
    Ok(())
}

fn read_document(path: &Path) -> Result<DocumentMut> {
    match fs::read_to_string(path) {
        Ok(text) => text
            .parse()
            .with_context(|| format!("parsing {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_document(path: &Path, doc: &DocumentMut) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, doc.to_string()).with_context(|| format!("writing {}", path.display()))
}

fn merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(base), Value::Object(over)) => {
            for (key, value) in over {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, value) => *base = value,
    }
}

fn merge_table(base: &mut Table, over: &Table) {
    for (key, value) in over {
        match (base.get_mut(key), value) {
            (Some(Item::Table(base)), Item::Table(over)) => merge_table(base, over),
            (Some(Item::Value(base)), Item::Value(over)) => {
                let decor = base.decor().clone();
                *base = over.clone();
                *base.decor_mut() = decor;
            }
            _ => {
                base.insert(key, value.clone());
            }
        }
    }
}
