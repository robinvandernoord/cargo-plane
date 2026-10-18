# cargo-plane

Cargo, but in the cloud(s). ✈️

`cargo plane <command>` syncs your workspace to a remote machine, runs cargo
there and streams the output back, so a weak laptop doesn't have to do the
compiling. Providers handle the remote's lifecycle: a plain SSH host, a
Fly.io Machine that starts on demand and stops itself when idle, or a GitHub
Codespace.

```sh
cargo plane check
cargo plane clippy -- -D warnings
cargo plane test
cargo plane build --release    # binaries are copied back to target/
cargo plane run -- --my-flag   # built remotely, runs locally
```

## Install

```sh
cargo install cargo-plane
```

Locally you need `ssh` and `rsync`. The remote needs `rsync`, a C linker
and a Rust toolchain (rustup's `~/.cargo/env` is sourced automatically).

## Configure

`.cargo/plane.toml` in your project, or `~/.config/cargo-plane/config.toml`
(project settings override global ones):

```toml
remote = "box"              # default remote; optional with only one

[remotes.box]
provider = "ssh"            # defaults to the remote's name
host = "my-build-box"       # hostname or ~/.ssh/config alias
# user = "me"
# port = 22
# remote_dir = "builds"     # relative to remote $HOME; default .cache/cargo-plane
# env = { RUSTFLAGS = "-C target-cpu=x86-64-v2" }
# ssh_options = ["-o", "Compression=yes"]
```

Pick another remote per invocation with `cargo plane --remote <name> ...` or
`CARGO_PLANE_REMOTE=<name>`.

### Fly.io

```sh
cargo plane setup fly
```

Run it inside a project: installs flyctl if needed, logs you in, creates a
throwaway build machine for that project that stops itself when idle, and adds
it to the project's `.cargo/plane.toml`. `cargo plane purge fly` deletes it. See
[`contrib/fly`](contrib/fly/README.md).

### GitHub Codespaces

```sh
cargo plane setup codespace
```

Run it inside a project with the [GitHub CLI](https://cli.github.com) installed.
It logs you in with the `codespace` scope if needed, creates a codespace from
the project's repository (or lets you pick an existing one), asks for a machine
type and idle timeout, and adds it to `.cargo/plane.toml`:

```toml
[remotes.codespace]
provider = "codespace"
codespace = "fluffy-space-waffle-q7xj9"   # from `gh codespace list`
# gh = "/path/to/gh"                       # default: gh on PATH
```

Then it installs rsync, a C compiler and rustup on the codespace if they're
missing, after asking. A Rust toolchain from the repository's dev container
(the `rust` feature or the `devcontainers/rust` image) is used as is. One that
setup installed lives in the codespace's home directory, which a rebuild of the
codespace wipes, so add the feature to `.devcontainer/devcontainer.json` if you
rebuild often:

```json
"features": { "ghcr.io/devcontainers/features/rust:1": {} }
```

SSH goes through `gh codespace ssh`, which starts a stopped codespace on demand;
the codespace needs an SSH server, which GitHub's default image has.
Builds live in `/workspaces/.cargo-plane`, which survives stops and rebuilds.
GitHub stops the codespace after its idle timeout and bills it per core-hour
while it runs, plus storage; personal accounts get a monthly allowance of both
for free. GitHub decides what counts as idle, so keep the timeout longer than
your slowest build. Stopped codespaces are deleted after their retention
period (30 days by default); run setup again to get a new one.
`cargo plane purge codespace` deletes it right away.

### Provider plugins

Any other provider can be a plugin: a binary named `cargo-plane-provider-<name>`
on your PATH. A remote with `provider = "<name>"` uses it, and
`cargo plane setup <name>` runs its setup, if it has one.

```sh
cargo install --path crates/cargo-plane-provider-wol   # Wake-on-LAN, in this repo
cargo plane setup wol
```

Plugins are written in Rust against the
[`cargo-plane-provider`](crates/cargo-plane-provider) crate. Implement its
`Provider` trait and hand it to `run`:

```rust
use cargo_plane_provider::{Endpoint, Host, Provider, run};

#[derive(serde::Deserialize)]
struct Config { host: String }

struct MyProvider;

impl Provider for MyProvider {
    type Config = Config;   // the remote's table from .cargo/plane.toml

    fn up(&self, config: &Config, host: &dyn Host) -> anyhow::Result<Endpoint> {
        host.status("Starting", &config.host);
        // start the machine...
        Ok(Endpoint::new(&config.host))
    }
    fn down(&self, config: &Config, host: &dyn Host) -> anyhow::Result<()> { todo!() }
    fn status(&self, config: &Config, host: &dyn Host) -> anyhow::Result<String> { todo!() }
    // optional: purge, boot_timeout, setup
}

fn main() -> std::process::ExitCode {
    run(MyProvider)
}
```

cargo-plane starts the plugin the first time a command needs the provider (not
on every build: the endpoint is cached) and talks to it over a Unix socket. The
plugin keeps the terminal, so tools it runs can prompt as usual. Questions and
status lines go through `Host`, so they look like the rest of cargo-plane.
During setup, `Host` can also save the remote's config and run commands on it.
The built-in providers implement the same trait; see
[`crates/cargo-plane-provider-wol`](crates/cargo-plane-provider-wol/src/main.rs)
for a complete plugin.

Plugins run with your permissions, like any program you install.

## Commands

Every cargo command runs remotely as-is, except:

| Command                     | Behaviour                                                        |
| --------------------------- | ---------------------------------------------------------------- |
| `build`                     | Also copies binaries and cdylib/dylib/staticlib files back       |
| `run`                       | Builds remotely, copies the binary back, runs it locally         |
| `test --local-run`          | Builds tests remotely, runs them locally (e.g. hardware tests)   |
| `takeoff` / `land`          | Start / stop the remote                                          |
| `setup fly`                 | Create a Fly.io build machine and configure it as a remote       |
| `setup codespace`           | Create or pick a GitHub Codespace and configure it as a remote   |
| `setup <plugin>`            | Run a provider plugin's setup                                    |
| `purge`                     | Permanently delete what `setup` created (asks you to type the name) |
| `status`                    | Show the remote's state                                          |

## rust-analyzer

Point rust-analyzer's checks at the remote too:

```json
"rust-analyzer.check.overrideCommand": ["cargo", "plane", "check", "--message-format=json"]
```

Paths in cargo's output are rewritten from the remote copy to your local
workspace, so diagnostics land on the right files.

## How it works

- The workspace is rsynced (respecting `.gitignore`, skipping `target/` and
  `.git/`) to `<remote_dir>/<name>-<hash>` on the remote. `target/` stays there,
  so builds are incremental.
- Cargo runs over SSH. Connections are multiplexed (`ControlMaster`), so after
  the first command each step costs about one round trip.
- If you interrupt cargo-plane or lose the connection, the remote cargo is
  stopped too.

## Caveats

- Binaries built remotely must run locally: same CPU architecture, and the
  remote's glibc must be the same as or older than yours. The remote needs the
  `-dev` packages your crates link against.
- Your local `~/.cargo/config.toml` isn't used remotely; the project's
  `.cargo/config.toml` is.
- `env!("CARGO_MANIFEST_DIR")` and other compile-time paths point at the
  remote copy.
- Path dependencies outside the workspace aren't synced.
- `test --local-run` doesn't run doctests.

## License

MIT
