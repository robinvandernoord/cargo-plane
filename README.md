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

With [cargo-binstall](https://github.com/cargo-bins/cargo-binstall) installed,
download a prebuilt binary:

```sh
cargo binstall cargo-plane
```

Release binaries include the SSH, Fly.io and Codespaces providers and are built
for Linux and macOS on x86_64 and ARM64. Linux binaries require glibc 2.35 or
newer (for example, Ubuntu 22.04). macOS binaries require macOS 15 or newer.

To build from source instead:

```sh
cargo install cargo-plane
```

Locally you need `ssh` and `rsync`. The remote needs `rsync`, a C linker
and a Rust toolchain (rustup's `~/.cargo/env` is sourced automatically).

### Releasing binaries

The `Release binaries` workflow builds archives when a `v<version>` tag is
pushed. The tag must match the version in `Cargo.toml`. After all four builds
succeed, the workflow attaches the archives to a GitHub release. Each archive
contains `cargo-plane`, `LICENSE` and `README.md`; the providers are compiled in.
The workflow does not publish crates to crates.io.

For the first release with binstall support, publish a new crate version that
includes the repository URL and binstall metadata. Publish any updated provider
dependencies before `cargo-plane`, and release the matching tag. Previously
published crate versions do not receive manifest changes.

You can also run the workflow manually from GitHub Actions to check all four
builds and download their archives without publishing a release. After both the
crate and its release assets are published, verify installation with:

```sh
cargo binstall cargo-plane --version <version> --strategies crate-meta-data
cargo plane --version
```

This checks the project's release assets without falling back to a source build
or third-party binaries.

## Configure

Run setup inside your Cargo project to create its configuration:

```sh
cargo plane setup local          # build on this machine
cargo plane setup ssh            # asks for an existing SSH hostname or alias
cargo plane setup ssh my-box --user robin --port 2222
cargo plane setup fly            # create a Fly.io machine
cargo plane setup codespace      # create or pick a GitHub Codespace
```

Use `cargo plane --remote <name> setup <provider>` to name the configuration.
All built-in providers accept `-y` / `--yes` to skip setup prompts. Local setup
always runs without prompts. SSH accepts a positional hostname or `--host`,
plus `--user` and `--port`; with `-y`, an existing hostname is reused if omitted.
SSH setup connects to an existing machine, checks its build tools, and offers
to install missing rsync, a C compiler and Rust. System packages need root or
passwordless sudo; Rust is installed in the remote account using rustup, with
clippy and rustfmt. Supported package managers are apt-get, dnf, yum, apk,
pacman and zypper. On other systems, install the missing tools manually and
rerun setup. Existing working tools are kept. For unattended provisioning, use
`cargo plane setup ssh my-box --yes`. Setup finishes by verifying the tools;
rerunning it preserves configuration and skips installation when tools work.

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

`.cargo/` is Cargo's own directory; cargo-plane only adds `plane.toml` to it.
When `cargo plane setup` creates that file, it also adds it to
`.cargo/.gitignore`, since the machines it describes are yours. Remove that line
to share a remote with your team. `.cargo/config.toml` isn't touched.

### Local execution

On a machine that can build locally, run `cargo plane setup local`, or select
the built-in `local` provider in your untracked `.cargo/plane.toml` (or global config):

```toml
remote = "local"

[remotes.local]
provider = "local"
```

Shared scripts can keep using `cargo plane check`, `cargo plane run`, and other
Cargo commands. The local provider runs Cargo directly in the current directory,
with the original arguments, terminal access and exit status. No SSH, syncing or
artifact copying is needed. Configured `env` values also apply locally.

`exec` runs a local command and `shell` opens your local `$SHELL` (or `/bin/sh`).
`test --local-run` runs ordinary local tests, with the redundant `--local-run`
flag removed. `takeoff` and `land` succeed without doing anything; `status`
reports local execution, and `purge` reports that there is nothing to delete.
Local setup only writes configuration and needs no SSH connection.
To switch between configured local and remote
providers, use `--remote <name>` or `CARGO_PLANE_REMOTE` as usual.

### Fly.io

```sh
cargo plane setup fly
```

Run it inside a project: installs flyctl if needed, logs you in, creates a
throwaway build machine for that project that stops itself when idle, and adds
it to the project's `.cargo/plane.toml`. `cargo plane purge fly` deletes it. See
[`crates/cargo-plane-provider-fly`](crates/cargo-plane-provider-fly/README.md).

For unattended setup, authenticate beforehand with `flyctl auth login` or
`FLY_API_TOKEN`, then run `cargo plane setup fly -y --region ams`. `--region`
is required with `-y`. Override choices with `--app`, `--org`, `--machine`,
`--ram`, `--volume-size`, `--idle-timeout` and `--ssh-key`; omitted choices
use the interactive defaults. Existing builders are reused unless `--rebuild`
is passed.

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

For unattended setup, authenticate GitHub with `codespace` access beforehand
(using `gh auth login --scopes codespace` or `GH_TOKEN`), then run:

```sh
cargo plane setup codespace -y
cargo plane setup codespace -y --ram 16
cargo plane setup codespace -y --codespace existing-codespace-name
cargo plane setup codespace -y --repo owner/repo --machine standardLinux32gb --idle-timeout 60
```

`-y` accepts creation and missing-tool installation, uses the detected repository,
prefers a 4-CPU machine (otherwise the largest available), and stops it after
30 idle minutes. `--ram` takes a positive whole number of GB and selects the
fewest CPUs with at least that much RAM, preferring less RAM when CPU counts
match. Use either `--ram` or `--machine`. `--devcontainer-path` selects a specific
devcontainer file in the local project and on GitHub; unattended creation
otherwise uses GitHub's default configuration and opts out of additional
repository permissions. Machine and idle timeout options only affect new
codespaces. An existing configured codespace is reused, while an unconfigured
remote creates a new one, matching the interactive default.
`--codespace <name>` explicitly selects an existing codespace instead.

Setup adds the Rust feature to the project's `.devcontainer/devcontainer.json`
or `.devcontainer.json`, preserving existing settings, comments and feature
options. If neither exists, it creates `.devcontainer/devcontainer.json` using
the universal dev container image. Use `--devcontainer-path` for an alternative
configuration; that file must exist locally. An existing Rust feature is kept,
including its version and options:

```json
"features": { "ghcr.io/devcontainers/features/rust:1": {} }
```

Commit and push the updated configuration to the repository branch used by the
codespace before creating or rebuilding it. Creation reads files from GitHub,
so local edits only take effect once pushed. For an existing codespace, rebuild
it after updating its checkout to install Rust through the feature.

Setup also checks the running codespace and offers to install missing rsync,
a C compiler and Rust. Existing toolchains are used as is. The rustup fallback
installs into the home directory to make the current container usable until
the feature is applied; a container rebuild removes that fallback installation.

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

Remote providers are plugins: binaries named `cargo-plane-provider-<name>`. A
remote with `provider = "<name>"` uses it, and `cargo plane setup <name>` runs
its setup, if it has one. The providers in this repo live in
[`crates/`](crates) and build such a binary each; cargo-plane also compiles
them in (Cargo features `ssh`, `fly` and `codespace`, on by default), so they
work without installing anything. For a provider that isn't built in,
cargo-plane looks on your PATH:

```sh
cargo install cargo-plane-provider-<name>
cargo plane setup <name>
```

To leave a provider out of cargo-plane and use its plugin binary instead:

```sh
cargo install cargo-plane --no-default-features --features ssh
cargo install cargo-plane-provider-fly
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
See [`crates/cargo-plane-provider-ssh`](crates/cargo-plane-provider-ssh/src/lib.rs)
for the smallest complete provider, and the Fly and Codespaces crates for ones
with a setup.

Plugins run with your permissions, like any program you install.

## Commands

Every cargo command runs remotely as-is, except:

| Command                     | Behaviour                                                        |
| --------------------------- | ---------------------------------------------------------------- |
| `build`                     | Also copies binaries and cdylib/dylib/staticlib files back       |
| `run`                       | Builds remotely, copies the binary back, runs it locally         |
| `test --local-run`          | Builds tests remotely, runs them locally (e.g. hardware tests)   |
| `fmt`                       | Runs locally: it's cheap, and remote changes would be overwritten |
| `clippy`                    | Adds the clippy component first if the remote toolchain lacks it |
| `fix`, `clippy --fix`       | Copies the fixed files back (needs a clean git tree, like cargo) |
| `add`, `remove`, `update`   | Copies `Cargo.toml` and `Cargo.lock` back                         |
| `exec <command>`            | Runs any command in the project's directory on the remote        |
| `shell`                     | Opens an interactive shell there                                 |
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

Commands that edit files (`fix`, `clippy --fix`, `add`, `remove`, `update`,
`generate-lockfile`) copy what they changed back. A file you edit locally while
such a command runs is kept as is; if cargo changed it too, cargo's version is
saved in `target/cargo-plane/conflicts/`. Before `fix` and `clippy --fix`,
cargo-plane checks for uncommitted changes locally, as cargo would (the remote
copy has no `.git`); `--allow-dirty` and `--allow-staged` work as usual.

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
