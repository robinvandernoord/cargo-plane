# cargo-plane

Cargo, but in the cloud(s). ✈️

`cargo plane <command>` syncs your workspace to a remote machine, runs cargo
there and streams the output back, so a weak laptop doesn't have to do the
compiling. Providers handle the remote's lifecycle: a plain SSH host, or a
Fly.io Machine that starts on demand and stops itself when idle.

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

## Commands

Every cargo command runs remotely as-is, except:

| Command                     | Behaviour                                                        |
| --------------------------- | ---------------------------------------------------------------- |
| `build`                     | Also copies binaries and cdylib/dylib/staticlib files back       |
| `run`                       | Builds remotely, copies the binary back, runs it locally         |
| `test --local-run`          | Builds tests remotely, runs them locally (e.g. hardware tests)   |
| `takeoff` / `land`          | Start / stop the remote                                          |
| `setup fly`                 | Create a Fly.io build machine and configure it as a remote       |
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
