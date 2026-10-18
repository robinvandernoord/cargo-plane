//! Providers manage a remote machine's lifecycle and say how to reach it.
//! Everything else (sync, running cargo, fetching artifacts) is shared.
//!
//! Built-in providers implement `cargo_plane_provider::Provider` like plugins
//! do; a provider that isn't built in is looked up as a plugin binary.

mod codespace;
mod fly;
mod plugin;
mod ssh;

use anyhow::{Result, bail};
pub use cargo_plane_provider::{AnyProvider, Endpoint};

struct Builtin {
    name: &'static str,
    /// What `setup` creates; `None` if it has no setup.
    setup: Option<&'static str>,
    make: fn() -> Box<dyn AnyProvider>,
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "ssh",
        setup: None,
        make: || Box::new(ssh::Ssh),
    },
    Builtin {
        name: "fly",
        setup: Some("a Fly.io machine that stops itself when idle"),
        make: || Box::new(fly::Fly),
    },
    Builtin {
        name: "codespace",
        setup: Some("a GitHub Codespace, or pick an existing one"),
        make: || Box::new(codespace::Codespace),
    },
];

/// Built-in providers with a setup, and what it creates.
pub fn builtin_setups() -> impl Iterator<Item = (&'static str, &'static str)> {
    BUILTINS.iter().filter_map(|b| Some((b.name, b.setup?)))
}

/// Names of the provider plugins on PATH.
pub fn plugins() -> Vec<String> {
    plugin::installed()
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
    bail!(
        "unknown provider `{name}`: it isn't built in ({}) and there's no `{}` on PATH",
        builtins.join(", "),
        plugin::binary_name(name)
    )
}
