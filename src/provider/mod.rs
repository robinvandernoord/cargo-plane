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
    make: fn() -> Box<dyn AnyProvider>,
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "ssh",
        make: || Box::new(ssh::Ssh),
    },
    Builtin {
        name: "fly",
        make: || Box::new(fly::Fly),
    },
    Builtin {
        name: "codespace",
        make: || Box::new(codespace::Codespace),
    },
];

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
