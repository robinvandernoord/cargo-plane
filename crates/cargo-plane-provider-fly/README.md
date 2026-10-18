# Fly.io build host

A throwaway build machine per project: create it with `cargo plane setup fly`
inside the project, delete it with `cargo plane purge fly` when you're done.

The image `setup` deploys runs sshd (on port 2222; Fly's own SSH server for
`fly ssh console` has 22), a Rust toolchain and an idle watchdog that stops
the machine after `IDLE_MINUTES` (default 15) without SSH connections. You
only pay for the machine while it runs, plus storage.

## Setup

```sh
cd my-project
cargo plane setup fly
```

This installs flyctl if it isn't on your PATH (into
`~/.local/share/cargo-plane/bin`, checksum-verified), logs you in, asks for
an app name (default `cargo-plane-<project>-<user>`), region, machine size,
RAM, volume size, idle timeout and SSH key, and then creates:

- the app,
- a volume `cargo_plane_data` mounted at `/data` (toolchain, cargo registry,
  synced projects and their `target/` dirs),
- a machine `cargo-plane-builder`, built from [`image/Dockerfile`](image/Dockerfile) on
  Fly's remote builders,

and adds `[remotes.fly]` to the project's `.cargo/plane.toml`. It's safe to
rerun; existing parts are reused. SSH goes through a background `flyctl proxy`,
so the machine doesn't need a public IP.

`cargo plane takeoff` / `land` start and stop the machine; other commands
start it when needed.

For unattended setup, authenticate with `flyctl auth login` beforehand or set
`FLY_API_TOKEN`, then pass `-y` and an explicit region:

```sh
cargo plane setup fly -y --region ams
cargo plane setup fly -y --region ams --ram 16
cargo plane setup fly -y --region ams --app my-builder --org personal \
  --machine performance-4x --ram 16 --volume-size 40 --idle-timeout 30 \
  --ssh-key ~/.ssh/id_ed25519
```

`-y` / `--yes` accepts flyctl installation and resource creation, uses defaults
for omitted choices, and never opens browser login. `--region` is required
with `-y`; the command errors before making changes if it is missing.
All setup choices have flags, which also work in interactive setup:

| Flag | Default |
| --- | --- |
| `--app <name>` | Configured app, otherwise `cargo-plane-<project>-<user>` |
| `--org <slug>` | Only available org, otherwise `personal` |
| `--region <code>` | Existing volume's region, otherwise asked interactively |
| `--machine <size>` | `performance-4x` |
| `--ram <size>` | Machine preset; accepts `16`, `16G`, `512M`, etc. |
| `--volume-size <GB>` | 20 |
| `--idle-timeout <minutes>` | 15 |
| `--ssh-key <path>` | First detected key pair, preferring standard SSH identities |

RAM must fit the selected machine's limits and be a multiple of 256 MB.
SSH key paths may point to either the private key or its `.pub` file; both
files must exist. Existing builders are reused unless `--rebuild` is passed,
so machine, RAM, idle timeout and SSH key choices apply when creating a builder.
Existing volumes are kept; `--volume-size` does not resize them and an explicit
`--region` must match their region. `--org` only applies when creating an app.

## Costs and removal

The machine is billed per second while it runs and stops itself after the idle
timeout. A stopped machine still costs its disk (~1 GB image) per GB-month,
and the volume is billed per GB-month until it's deleted; the machine stopping
doesn't delete it. Volume snapshots are turned off, since it's only a cache.

`cargo plane purge fly` deletes the app with its machine and volume (after you
type the app name) and removes `[remotes.fly]` from the project's config.

## Customizing the image

The image is Debian trixie, so binaries built here run on systems with glibc
2.41 or newer. To add the `-dev` packages your crates link against:

```sh
cargo plane setup fly --eject ./fly-builder
$EDITOR ./fly-builder/Dockerfile
cargo plane setup fly --rebuild --dockerfile ./fly-builder/Dockerfile
```

`--rebuild` recreates the machine; the volume and its caches are kept.

## Config

```toml
[remotes.fly]
provider = "fly"
app = "cargo-plane-you-1a2b"
# machine = "<id>"          # default: the machine tagged by setup
# proxy_port = 23456        # default: derived from the app name
# ssh_host = "<id>.vm.<app>.internal"  # skip the proxy if you're on Fly's WireGuard network
# flyctl = "/path/to/flyctl"
```
