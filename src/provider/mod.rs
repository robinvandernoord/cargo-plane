//! Providers manage a remote machine's lifecycle and say how to reach it.
//! Everything else (sync, running cargo, fetching artifacts) is shared.
//!
//! Each provider is its own crate (`crates/cargo-plane-provider-*`) that
//! builds a plugin binary. The ones enabled as features are also compiled in;
//! any other provider is looked up as a plugin binary on PATH.

mod plugin;

use anyhow::{Result, bail};
pub use cargo_plane_provider::{AnyProvider, Endpoint};

struct Builtin {
    name: &'static str,
    /// What `setup` creates; `None` if it has no setup.
    setup: Option<&'static str>,
    make: fn() -> Box<dyn AnyProvider>,
}

const BUILTINS: &[Builtin] = &[
    #[cfg(feature = "ssh")]
    Builtin {
        name: "ssh",
        setup: None,
        make: || Box::new(cargo_plane_provider_ssh::Ssh),
    },
    #[cfg(feature = "fly")]
    Builtin {
        name: "fly",
        setup: Some("a Fly.io machine that stops itself when idle"),
        make: || Box::new(cargo_plane_provider_fly::Fly),
    },
    #[cfg(feature = "codespace")]
    Builtin {
        name: "codespace",
        setup: Some("a GitHub Codespace, or pick an existing one"),
        make: || Box::new(cargo_plane_provider_codespace::Codespace),
    },
];

/// Built-in providers with a setup, and what it creates.
pub fn builtin_setups() -> impl Iterator<Item = (&'static str, &'static str)> {
    BUILTINS.iter().filter_map(|b| Some((b.name, b.setup?)))
}

/// Names of the provider plugins on PATH that aren't built in.
pub fn plugins() -> Vec<String> {
    let mut plugins = plugin::installed();
    plugins.retain(|name| !BUILTINS.iter().any(|b| b.name == name));
    plugins
}

/// The built-in provider called `name`, or else the plugin binary for it.
pub fn lookup(name: &str) -> Result<Box<dyn AnyProvider>> {
    if let Some(builtin) = BUILTINS.iter().find(|b| b.name == name) {
        return Ok((builtin.make)());
    }
    if let Some(plugin) = plugin::Plugin::locate(name) {
        return Ok(Box::new(plugin));
    }
    let builtins: Vec<_> = BUILTINS.iter().map(|b| b.name).collect();
    let builtins = match builtins.as_slice() {
        [] => "this cargo-plane has none built in".to_owned(),
        names => format!("built in: {}", names.join(", ")),
    };
    bail!(
        "unknown provider `{name}` ({builtins}) and there's no `{}` on PATH",
        plugin::binary_name(name)
    )
}
