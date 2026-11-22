# Container build host

A Docker or Podman container on a machine you can SSH into. Builds use that
machine's CPU but the image's userland, so binaries link against the image's
glibc, not the host's. That matters when the host runs a newer distribution
than the laptop that runs the binaries: with the plain `ssh` provider, a build
on Ubuntu 26.04 produces binaries an Ubuntu 24.04 laptop can't load.

## Setup

```sh
cd my-project
cargo plane setup container my-box
```

`my-box` is a hostname or `~/.ssh/config` alias, reachable without prompts
(an SSH key, not a password). The host needs Docker (with your user in the
`docker` group) or Podman. Setup asks for a base image and an
idle timeout, then:

- creates an SSH key pair in `~/.local/share/cargo-plane/container/`, shared
  by all containers made on this machine,
- builds an image `cargo-plane/<container>` on the host from
  [`image/Dockerfile`](image/Dockerfile): the base image plus rsync, git, a C
  toolchain, sshd and Rust (with clippy and rustfmt) installed by rustup,
- creates a container `cargo-plane-<project>-<hash>` with three volumes:
  synced projects and their `target/` dirs, the cargo registry and git
  checkouts,

and adds `[remotes.container]` to the project's `.cargo/plane.toml`:

```toml
[remotes.container]
provider = "container"
host = "my-box"
engine = "docker"                         # or "podman"
container = "cargo-plane-my-project-1a2b3c"
image = "ubuntu:24.04"                    # the base the image was built from
packages = []                             # extra apt packages
idle_minutes = 0                          # 0: never stop by itself
# user = "me"
# port = 22
```

The base image must be Debian or Ubuntu. Pick one no newer than the oldest
system the binaries have to run on; `ubuntu:22.04`, for example, gives glibc
2.35. Crates that link system libraries need their `-dev` packages in the
image: `--packages libssl-dev,protobuf-compiler`.

Rerunning setup is safe. An existing container is reused unless the base
image, packages or idle timeout differ from what it was made with, or
`--rebuild` is passed. Those rebuild the image and recreate the container,
keeping the cargo registry and git caches. The work volume (synced projects
and `target/`) is kept too, unless it holds builds for another base image or
package list: Cargo doesn't notice a different glibc or linker, so it would
otherwise reuse binaries built in the old userland. The volume carries a label
saying which userland it's for, so this also works when only the volume is
left of an earlier container. It also records which machine's key made it:
setup never empties a volume it didn't make (another project's, say, after a
hand-picked `--container` name) unless you pass `--rebuild`. Setup compares with a label on the
container, not with the saved config, so a rerun after a failed setup retries
instead of reusing the old container.

The default container name includes this machine's hostname and the remote's
name, so different machines (CI runners, say) and differently named remotes
on one host each get their own container. Setup also won't replace a
container that was made with another SSH key without `--rebuild`. That also
covers a lost key pair: rerun setup with `--rebuild` to make a new one and
rebuild the container with it. A lost public key alone is restored from the
private key.

Setup won't quietly point an existing remote at a different host, engine or
container: the old container and its volumes would stay behind without
cargo-plane knowing about them. It explains how to purge them first or use
another remote name, and only continues if you confirm in a terminal.

For unattended setup, make sure `ssh my-box` works without prompts, then:

```sh
cargo plane setup container my-box -y
cargo plane setup container my-box -y --image ubuntu:22.04 --packages libssl-dev
cargo plane setup container my-box -y --engine podman --idle-timeout 30
```

## How it connects

The container runs no SSH server and publishes no ports. cargo-plane's SSH
connections go through a ProxyCommand that logs in to the host and runs
`sshd -i` in the container with `docker exec -i`: an SSH server for exactly
one connection, over stdin and stdout. The ProxyCommand doesn't start a
stopped container, so `cargo plane status` reports it as stopped instead of
waking it up. Other commands find the cached connection failing, and start
the container before connecting again.

Inside, builds run as root in `/work`.

## Lifecycle

`cargo plane takeoff` / `land` start and stop the container; other commands
start it when needed, in about a second. Stopping is instant (the container
runs with `--init`, so signals reach its processes).

An idle container is a sleeping shell, so by default it never stops by
itself. With `--idle-timeout <min>`, it stops once nothing has run in it for
that many minutes. cargo-plane keeps its SSH connection open for 10 minutes
after the last command, which counts as running, so the effective timeout is
up to 10 minutes longer.

The container is created with `--restart no`: after the host reboots, it stays
stopped until the next cargo-plane command starts it.

`cargo plane purge container` lists what's left of the container, its image
and its volumes, and deletes them after you type the container name, even if
only leftovers remain. If something can't be deleted (a volume still in use,
for example), purge fails and keeps the remote's config so you can retry. The
shared key pair stays.

## Security

Being in the `docker` group is equivalent to root on the host. Rootless Podman
avoids that and works the same way.

The container's SSH host keys are not checked: the connection to the host is
already verified by your SSH configuration, and the container's keys change
with every image rebuild.
