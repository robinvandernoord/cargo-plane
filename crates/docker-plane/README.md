# docker-plane

Docker builds on remote machines, using the same SSH, Fly.io and GitHub
Codespaces provider crates as cargo-plane. Docker Buildx transfers the build
context, streams progress and exports the result. docker-plane starts the
machine and connects Buildx to it.

## Install

From this checkout:

```sh
cargo install --path crates/docker-plane
```

The standalone command works immediately:

```sh
docker-plane setup ssh my-build-box
docker-plane build --load -t my-app .
```

To also use `docker plane`, register the binary as a Docker CLI plugin:

```sh
mkdir -p ~/.docker/cli-plugins
ln -s "$(command -v docker-plane)" ~/.docker/cli-plugins/docker-plane
docker plane --help
```

With a custom `DOCKER_CONFIG`, use its `cli-plugins` directory instead.
Do not overwrite an existing plugin. The binary implements Docker's metadata
protocol; `docker plugin install` manages a different kind of plugin.

Requires a Unix client with the Docker CLI, Buildx and OpenSSH. A local Docker
engine is needed for `--load`, but not for remote builds that use `--push` or
leave their output cached remotely.

## Build

```sh
docker plane setup fly
docker plane build --load -t my-app .
docker plane build --push -t ghcr.io/me/my-app .
docker plane build --platform linux/amd64,linux/arm64 --push -t ghcr.io/me/my-app .
docker plane bake -f docker-bake.hcl
docker plane --remote box build --load -t my-app .
```

Build and Bake arguments pass through to Buildx, including cache, secret,
SSH-agent, output and progress options. docker-plane owns `--builder`; select
a machine with `--remote` before the subcommand instead. Multi-platform builds
need native builder support or emulation configured on the remote, just as
with ordinary Buildx. `--load` imports into the engine selected by your local
Docker context. Without `--load`, `--push` or another output, results remain
in the remote build cache. docker-plane does not choose an output for you.

Docker global flags before `plane`, such as `docker --config <dir> plane ...`,
are forwarded to Docker subprocesses. Build arguments retain their original
argument boundaries. Help and metadata requests do not start a machine.

## Configuration

`.docker/plane.toml`, searched upward from the current directory, is merged
over `~/.config/docker-plane/config.toml` (`XDG_CONFIG_HOME` is respected):

```toml
remote = "box"

[remotes.box]
provider = "ssh"
host = "my-build-box"
# user = "builder"
# port = 2222
# ssh_options = ["-i", "/home/me/.ssh/build-key"]

[remotes.fly]
provider = "fly"
app = "docker-plane-my-app"

[remotes.codespace]
provider = "codespace"
codespace = "my-codespace-name"
```

Selection order is `--remote`, `DOCKER_PLANE_REMOTE`, `remote` in the merged
configuration, then the only configured remote. Provider names default to
their remote name. Remote names allow letters, digits, underscores and hyphens.

Setup writes project configuration and adds `plane.toml` to `.docker/.gitignore`.
Remove that ignore entry to share the configuration. Setup also records
`build_environment = "docker"` so shared providers choose the Docker build
environment. Cargo's `.cargo/plane.toml` is independent.

The existing provider crate names are retained for compatibility. This initial
Docker frontend includes the three built-in providers; external provider
executables are not yet discovered by docker-plane.

## Providers

### SSH

```sh
docker plane setup ssh my-build-box --user builder --port 22
```

Install and start Docker on the host first, and allow the SSH user to access
its Docker socket. Setup verifies `docker info`, then Buildx creates a persistent
BuildKit container. It does not install Rust or rsync, change Docker permissions,
or replace a host's existing daemon. `--yes` is accepted for compatibility with
the SSH provider, but does not install Docker automatically.

### Fly.io

```sh
docker plane setup fly
```

Reuses the Fly provider's login, region/size selection, SSH keys, volume and
Machine lifecycle. The default app name starts with `docker-plane-`, separate
from Cargo builder apps. The Docker image runs dockerd and SSH on port 2222,
with `/data/docker` on the persistent volume. BuildKit containers and their
layer caches survive Machine stops and image rebuilds.

An open SSH session keeps the Machine awake throughout a build; closing the
connection lets the remote idle watchdog stop it. The local client does not
leave an SSH control master running afterwards. The provider's lightweight
`fly proxy` process may remain for subsequent connections.

```sh
docker plane setup fly --rebuild
docker plane setup fly --eject ./fly-docker-builder
docker plane setup fly --dockerfile ./fly-docker-builder/Dockerfile --rebuild
```

Custom images must provide Docker access and the same SSH/key/volume contract
as the default image. `--rebuild` keeps the volume. Creating Fly resources uses
the provider's existing interactive cost review and confirmation.

### GitHub Codespaces

```sh
docker plane setup codespace
```

Uses `gh` to create or select a codespace and obtain its SSH configuration.
The codespace must provide a running Docker daemon accessible to its SSH user.
For a custom devcontainer, add Docker-in-Docker and SSH support before creating
or rebuilding it:

```json
"features": {
  "ghcr.io/devcontainers/features/docker-in-docker:4": {},
  "ghcr.io/devcontainers/features/sshd:1": {}
}
```

Setup checks Docker access and explains what to configure if it is missing;
it does not edit or commit your devcontainer configuration. Docker-in-Docker's
storage behavior depends on that devcontainer configuration. Stopping and
starting normally retains the cache; rebuilding the devcontainer can reset it.
GitHub's idle timeout and codespace retention policy still apply.

## Lifecycle

```sh
docker plane takeoff           # start the machine and prepare BuildKit
docker plane status            # inspect provider state without starting it
docker plane land              # stop the machine, keeping its cache
docker plane purge             # delete resources after provider confirmation
docker plane status fly        # select a remote for a lifecycle command
```

`takeoff` returns after preparing the builder and closes its SSH session; normal
idle shutdown still applies. Each build reconnects and ensures the machine is
running. Buildx registrations are named `docker-plane-<configuration hash>` and
do not change the selected Docker context or default Buildx builder. Provider
SSH options are applied only to Docker subprocesses through a temporary SSH
launcher, without editing `~/.ssh/config`. Direct use of the generated builder
outside docker-plane does not carry this SSH configuration or start its machine.

Fly purge deletes the whole app and volume. Codespaces purge deletes the whole
codespace, including any work in it. SSH purge only forgets the configuration
and local Buildx registration; it leaves the host and its Docker data intact.
If the remote overrides a global entry, purge removes the project entry first.

For ordinary Docker execution with no remote lifecycle:

```sh
docker plane setup local
docker plane build --load -t my-app .
```

The local provider uses Buildx in your current Docker context. Its lifecycle
commands do not start or stop any machine.
