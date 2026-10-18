//! Prepare an existing SSH account for remote Rust builds.

use anyhow::{Result, bail};
use cargo_plane_provider::Host;

const CHECK_TOOLS: &str = r#"
for tool in rsync cc curl; do
    command -v "$tool" >/dev/null 2>&1 || printf '%s\n' "$tool"
done
cargo --version >/dev/null 2>&1 || echo cargo
rustc --version >/dev/null 2>&1 || echo rustc
"#;

// No terminal is attached to run_remote. Fail with an actionable explanation
// rather than letting sudo ask for a password on a closed stdin.
const INSTALL_PACKAGES: &str = r#"
set -eu
if [ "$(id -u)" -eq 0 ]; then
    plane_sudo=
elif command -v sudo >/dev/null 2>&1 && sudo -n true; then
    plane_sudo='sudo -n'
else
    echo 'Installing build packages requires root or passwordless sudo. Install rsync, a C compiler, curl and CA certificates manually, then rerun setup.' >&2
    exit 1
fi
if command -v apt-get >/dev/null 2>&1; then
    $plane_sudo apt-get update
    $plane_sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y rsync build-essential curl ca-certificates pkg-config
elif command -v dnf >/dev/null 2>&1; then
    $plane_sudo dnf install -y rsync gcc gcc-c++ make curl ca-certificates pkgconf-pkg-config
elif command -v yum >/dev/null 2>&1; then
    $plane_sudo yum install -y rsync gcc gcc-c++ make curl ca-certificates pkgconfig
elif command -v apk >/dev/null 2>&1; then
    $plane_sudo apk add rsync build-base curl ca-certificates pkgconf
elif command -v pacman >/dev/null 2>&1; then
    $plane_sudo pacman -S --needed --noconfirm rsync base-devel curl ca-certificates pkgconf
elif command -v zypper >/dev/null 2>&1; then
    $plane_sudo zypper --non-interactive install rsync gcc gcc-c++ make curl ca-certificates pkg-config
else
    echo 'No supported package manager found. Install rsync, a C compiler, curl and CA certificates manually, then rerun setup.' >&2
    exit 1
fi
"#;

const INSTALL_RUST: &str = r#"
set -eu
# Use one IO thread/download and 256 MiB of unpacking buffers on small hosts.
# Respect explicit overrides. See rustup's documented environment variables.
export RUSTUP_IO_THREADS="${RUSTUP_IO_THREADS:-1}"
export RUSTUP_UNPACK_RAM="${RUSTUP_UNPACK_RAM:-268435456}"
export RUSTUP_CONCURRENT_DOWNLOADS="${RUSTUP_CONCURRENT_DOWNLOADS:-1}"
if command -v rustup >/dev/null 2>&1; then
    rustup toolchain install stable --profile minimal --component clippy --component rustfmt
    rustup default stable
else
    plane_install_dir=$(mktemp -d)
    trap 'rm -rf "$plane_install_dir"' EXIT HUP INT TERM
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o "$plane_install_dir/rustup-init.sh"
    sh "$plane_install_dir/rustup-init.sh" -y --profile minimal --component clippy --component rustfmt
fi
"#;

pub fn provision(host: &dyn Host, yes: bool) -> Result<()> {
    host.status("Checking", "remote build tools");
    let output = host.run_remote(CHECK_TOOLS, true)?;
    if !output.success {
        bail!("checking remote tools failed: {}", output.stderr.trim());
    }
    let missing: Vec<_> = output.stdout.split_whitespace().collect();
    let needs_rust = missing.contains(&"cargo") || missing.contains(&"rustc");
    let needs_packages = missing.contains(&"rsync")
        || missing.contains(&"cc")
        || (needs_rust && missing.contains(&"curl"));
    if needs_packages {
        approve(
            host,
            yes,
            "Install remote build packages (rsync, C compiler, curl and CA certificates)?",
        )?;
        host.status("Installing", "remote build packages");
        run(host, INSTALL_PACKAGES, "installing remote build packages")?;
    }
    if needs_rust {
        approve(
            host,
            yes,
            "Install a Rust toolchain with clippy and rustfmt using rustup in the remote account?",
        )?;
        host.status("Installing", "remote Rust toolchain");
        run(host, INSTALL_RUST, "installing the remote Rust toolchain")?;
    }
    run(
        host,
        "rsync --version >/dev/null && cc --version >/dev/null && cargo --version && rustc --version",
        "verifying remote build tools",
    )
}

fn approve(host: &dyn Host, yes: bool, question: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !host.interactive() {
        bail!("{question} Rerun setup in a terminal, or pass --yes to install without prompts");
    }
    if !host.confirm(question, true)? {
        bail!("remote build tools are missing; install them manually and rerun setup");
    }
    Ok(())
}

fn run(host: &dyn Host, command: &str, action: &str) -> Result<()> {
    let output = host.run_remote(command, false)?;
    if !output.success {
        bail!(
            "{action} failed (exit code {:?}); fix the reported error and rerun setup",
            output.code
        );
    }
    Ok(())
}
