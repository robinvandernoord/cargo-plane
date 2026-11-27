//! `fast = true`: quicker rebuilds from tools that are stable today, without
//! touching the project's toolchain or files.
//!
//! - **The wild linker** on Linux (x86-64, AArch64, RISC-V). Linking is most
//!   of a rebuild after a small edit; wild halves it compared with rust-lld.
//!   A pinned, checksummed static build is installed in
//!   `$CARGO_HOME/cargo-plane/` the first time it's needed, and selected
//!   with `-B` (gcc and clang look for `ld` there), after turning off
//!   rustc's own rust-lld (1.90+ on x86-64).
//! - **Line tables only** for dev debug info, unless the project picks its
//!   own: backtraces keep files and lines, at a fraction of the size.
//!
//! Both are passed as `cargo --config` arguments, which cargo merges with the
//! project's own config (unlike `RUSTFLAGS`, which would replace its
//! rustflags). Whatever can't be had (no network, another OS) is left out,
//! so a build never fails because of fast mode.
//!
//! Measured on nushell (32 cores): a rebuild after editing the binary crate
//! went from 2.0s to 0.9s; Cranelift and the parallel front end gained
//! little more and need nightly, so they're not used.

use std::path::Path;

use crate::util::shell_quote;

const WILD_VERSION: &str = "0.11.0";

/// `uname -m` patterns, the release's architecture and the sha256 of its
/// static (musl) archive.
const WILD_BUILDS: &[(&str, &str, &str)] = &[
    (
        "x86_64|amd64",
        "x86_64",
        "71abbcb7264a31571f3eeb1b7342b0dd6e0ddbcaf280bcfe7e427c0a2bead79f",
    ),
    (
        "aarch64|arm64",
        "aarch64",
        "4b3696f6b030a03d4e903abe267c2c858b22d03df53998d96d4a166a074dede5",
    ),
    (
        "riscv64",
        "riscv64gc",
        "0948db20e123ee2d1b69f174c116eff48cc2896d34a52789705ab351b59a3d3b",
    ),
];

/// Shell lines that set `"$@"` to the `--config` arguments for cargo.
/// `toolchain` is a `+toolchain` the command was given, if any, so the rustc
/// version check sees the toolchain cargo will use. With `debug`, dev
/// builds get line tables only.
pub fn script(toolchain: Option<&str>, debug: bool) -> String {
    let mut script = String::from("set --\n");
    script += &wild_installer();
    let plus = toolchain
        .map(|t| format!(" {}", shell_quote(&format!("+{t}"))))
        .unwrap_or_default();
    // A path with a quote or backslash would need escaping in TOML; such a
    // $CARGO_HOME just doesn't get wild.
    script += &format!(
        r#"if plane_wild=$(plane_install_wild); then
    case "$plane_wild" in *[\"\\]*) plane_wild= ;; esac
fi
if [ -n "$plane_wild" ]; then
    plane_flags="\"-Clink-arg=-B$plane_wild\""
    case "$(uname -m)" in
        x86_64|amd64)
            plane_arch=x86_64
            # rustc 1.90+ links with its own rust-lld on x86-64 Linux.
            plane_minor=$(rustc{plus} -V 2>/dev/null | sed -n 's/^rustc 1\.\([0-9][0-9]*\)\..*/\1/p')
            if [ "${{plane_minor:-0}}" -ge 90 ]; then
                plane_flags="\"-Clinker-features=-lld\", $plane_flags"
            fi ;;
        aarch64|arm64) plane_arch=aarch64 ;;
        *) plane_arch=riscv64 ;;
    esac
    set -- "$@" --config "target.'cfg(all(target_os = \"linux\", target_env = \"gnu\", target_arch = \"$plane_arch\"))'.rustflags = [$plane_flags]"
fi
"#
    );
    if debug {
        script += "set -- \"$@\" --config 'profile.dev.debug = \"line-tables-only\"'\n";
    }
    script
}

/// `program args...` for the shell, with fast mode's `"$@"` after the
/// `+toolchain`, if any (cargo only takes it first).
pub fn command(program: &str, args: &[String]) -> String {
    let toolchain = usize::from(args.first().is_some_and(|a| a.starts_with('+')));
    let (plus, rest) = args.split_at(toolchain);
    let mut words = vec![shell_quote(program)];
    words.extend(plus.iter().map(|a| shell_quote(a)));
    words.push("\"$@\"".into());
    words.extend(rest.iter().map(|a| shell_quote(a)));
    words.join(" ")
}

/// A shell function printing the directory that holds wild as `ld`,
/// installing it first if needed; it fails where wild can't be had. A failed
/// download is retried after a day, not on every command.
fn wild_installer() -> String {
    let mut arches = String::new();
    for (pattern, arch, sha) in WILD_BUILDS {
        arches += &format!("        {pattern}) a={arch} h={sha} ;;\n");
    }
    format!(
        r#"plane_install_wild() {{
    [ "$(uname -s)" = Linux ] || return 1
    case "$(uname -m)" in
{arches}        *) return 1 ;;
    esac
    d="${{CARGO_HOME:-$HOME/.cargo}}/cargo-plane/wild-{WILD_VERSION}"
    [ -x "$d/ld" ] && {{ printf '%s\n' "$d"; return 0; }}
    [ -n "$(find "$d.failed" -mmin -1440 2>/dev/null)" ] && return 1
    mkdir -p "${{d%/*}}" || return 1
    n="wild-linker-{WILD_VERSION}-$a-unknown-linux-musl"
    u="https://github.com/davidlattimore/wild/releases/download/{WILD_VERSION}/$n.tar.gz"
    t=$(mktemp -d "$d.XXXXXX") || return 1
    echo "cargo-plane: installing the wild linker {WILD_VERSION} for fast mode" >&2
    if {{ curl -fsSL --retry 2 -o "$t/w.tgz" "$u" 2>/dev/null || wget -q -O "$t/w.tgz" "$u"; }} \
        && [ "$(sha256sum "$t/w.tgz" | cut -d' ' -f1)" = "$h" ] \
        && tar -xzf "$t/w.tgz" -C "$t" \
        && mkdir "$t/bin" && mv "$t/$n/wild" "$t/bin/wild" && ln -s wild "$t/bin/ld" \
        && "$t/bin/wild" --version >/dev/null 2>&1 \
        && {{ mv "$t/bin" "$d" 2>/dev/null || [ -x "$d/ld" ]; }}; then
        rm -rf "$t" "$d.failed"
        printf '%s\n' "$d"
    else
        rm -rf "$t"
        : > "$d.failed"
        echo "cargo-plane: couldn't install the wild linker; using the default linker (retried in a day)" >&2
        return 1
    fi
}}
"#
    )
}

/// Whether the project chose its own dev debug info, in the workspace's
/// `Cargo.toml` or its `.cargo/config.toml`; fast mode leaves that alone.
/// (`CARGO_PROFILE_DEV_DEBUG` in the remote's `env` is the caller's to check.)
pub fn project_sets_debug(root: &Path) -> bool {
    let read = |path: &Path| -> Option<toml::Table> {
        toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
    };
    let in_profile = |table: &toml::Table| {
        table
            .get("profile")
            .and_then(|p| p.get("dev"))
            .and_then(|d| d.get("debug"))
            .is_some()
    };
    [root.join("Cargo.toml"), root.join(".cargo/config.toml")]
        .iter()
        .filter_map(|path| read(path))
        .any(|table| in_profile(&table))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_shape() {
        let s = script(Some("nightly"), true);
        assert!(s.starts_with("set --\n"), "{s}");
        assert!(
            s.contains("rustc '+nightly' -V") || s.contains("rustc +nightly -V"),
            "{s}"
        );
        assert!(s.contains("profile.dev.debug"), "{s}");
        assert!(!script(None, false).contains("profile.dev.debug"));
        for (_, arch, sha) in WILD_BUILDS {
            assert!(s.contains(&format!("a={arch} h={sha}")), "{s}");
        }
    }

    #[test]
    fn config_goes_after_the_toolchain() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            command("cargo", &args(&["+nightly", "build", "--bin", "x y"])),
            r#"cargo +nightly "$@" build --bin 'x y'"#
        );
        assert_eq!(command("cargo", &args(&["check"])), r#"cargo "$@" check"#);
    }

    #[test]
    fn finds_projects_own_debug_setting() {
        let dir = std::env::temp_dir().join(format!("cargo-plane-fast-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".cargo")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert!(!project_sets_debug(&dir));
        std::fs::write(dir.join(".cargo/config.toml"), "[profile.dev]\ndebug = 2\n").unwrap();
        assert!(project_sets_debug(&dir));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
