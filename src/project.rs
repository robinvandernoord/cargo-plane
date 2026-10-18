//! The local workspace and its mirror on the remote: sync, run cargo, fetch.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

use crate::artifacts::{self, Artifact, Message};
use crate::cargo_args;
use crate::output::{replace, rewrite_stream};
use crate::remote::{Conn, env_prelude};
use crate::ui;
use crate::util::{self, shell_join, shell_quote};

/// What isn't synced: version control, build output, what git ignores.
const SYNC_FILTERS: &[&str] = &[
    "--exclude=/.git/",
    "--exclude=/target/",
    "--filter=:- .gitignore",
];

/// Local variables passed on to remote cargo.
const FORWARDED_ENV: &[&str] = &[
    "CARGO_TERM_COLOR",
    "CARGO_TERM_VERBOSE",
    "CARGO_TERM_QUIET",
    "RUST_BACKTRACE",
    "RUST_LOG",
    "RUST_TEST_THREADS",
];

/// Root directory of the cargo workspace around the current directory
/// (or around `manifest`).
pub fn workspace_root(manifest: Option<&str>) -> Result<PathBuf> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.args(["locate-project", "--workspace", "--message-format", "plain"]);
    if let Some(manifest) = manifest {
        cmd.args(["--manifest-path", manifest]);
    }
    let output = cmd
        .stderr(Stdio::null())
        .output()
        .context("running cargo locate-project")?;
    if !output.status.success() {
        bail!("not inside a cargo workspace");
    }
    let manifest = PathBuf::from(String::from_utf8(output.stdout)?.trim());
    Ok(manifest
        .parent()
        .context("manifest has no parent dir")?
        .to_path_buf())
}

pub struct Project {
    pub local_root: PathBuf,
    local_root_str: String,
    remote_root: String,
    /// Current directory relative to the workspace root.
    subdir: String,
    name: String,
}

pub enum Stdout {
    /// Copy stdout through (with paths rewritten).
    Passthrough,
    /// Collect artifacts from JSON messages; `echo` also prints them.
    Artifacts { echo: bool },
}

pub struct Outcome {
    pub code: i32,
    pub artifacts: Vec<Artifact>,
}

impl Project {
    /// Find the workspace root for the current directory (or `--manifest-path`).
    pub fn locate(args: &[String], conn: &Conn) -> Result<Self> {
        let local_root = workspace_root(cargo_args::flag_value(args, "--manifest-path"))?;
        let local_root_str = local_root
            .to_str()
            .context("workspace path is not valid UTF-8")?
            .to_owned();

        let cwd = std::env::current_dir()?;
        let subdir = cwd
            .strip_prefix(&local_root)
            .ok()
            .and_then(Path::to_str)
            .unwrap_or("")
            .to_owned();

        let name: String = local_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "._-".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        // The hash keeps same-named projects from different paths apart.
        let hash = util::fnv1a(local_root_str.as_bytes()) as u32;
        let remote_root = format!("{}/{name}-{hash:08x}", conn.base_dir());

        Ok(Self {
            local_root,
            local_root_str,
            remote_root,
            subdir,
            name,
        })
    }

    pub fn sync(&self, conn: &Conn) -> Result<()> {
        ui::status("Syncing", format!("{} to {}", self.name, conn.label()));
        let status = conn
            .rsync()
            .args(["--archive", "--delete", "--compress"])
            .args(SYNC_FILTERS)
            .arg(format!(
                "--rsync-path=mkdir -p {} && rsync",
                shell_quote(&self.remote_root)
            ))
            .arg(format!("{}/", self.local_root_str))
            .arg(conn.rsync_path(&format!("{}/", self.remote_root)))
            .stdin(Stdio::null())
            .status()
            .context("running rsync")?;
        if !status.success() {
            bail!("syncing to {} failed ({status})", conn.label());
        }
        Ok(())
    }

    /// Run cargo on the remote, streaming its output with paths rewritten.
    pub fn cargo(&self, conn: &Conn, args: &[String], stdout: Stdout) -> Result<Outcome> {
        self.run(conn, "cargo", args, stdout)
    }

    /// Run `program` on the remote in the current directory's counterpart,
    /// streaming its output with paths rewritten.
    pub fn run(
        &self,
        conn: &Conn,
        program: &str,
        args: &[String],
        stdout: Stdout,
    ) -> Result<Outcome> {
        let args: Vec<String> = args
            .iter()
            .map(|a| a.replace(&self.local_root_str, &self.remote_root))
            .collect();
        let script = self.script(conn, program, &args)?;

        let mut child = conn
            .ssh()
            .arg(format!("sh -c {}", shell_quote(&script)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running ssh")?;
        // Held open for the whole run: the remote side kills cargo when it closes.
        let stdin = child.stdin.take();

        let (from, to) = (
            self.remote_root.clone().into_bytes(),
            self.local_root_str.clone().into_bytes(),
        );
        let stderr = child.stderr.take().unwrap();
        let stderr_thread = {
            let (from, to) = (from.clone(), to.clone());
            thread::spawn(move || rewrite_stream(stderr, io::stderr(), &from, &to))
        };

        let child_stdout = child.stdout.take().unwrap();
        let mut artifacts = Vec::new();
        match stdout {
            Stdout::Passthrough => rewrite_stream(child_stdout, io::stdout(), &from, &to)?,
            Stdout::Artifacts { echo } => {
                let mut reader = BufReader::new(child_stdout);
                let mut out = io::stdout();
                let mut line = Vec::new();
                while reader.read_until(b'\n', &mut line)? > 0 {
                    let rewritten = replace(&line, &from, &to);
                    match artifacts::parse_line(&String::from_utf8_lossy(&rewritten)) {
                        Some(Message::Artifact(artifact)) => {
                            artifacts.push(artifact);
                            if echo {
                                out.write_all(&rewritten)?;
                            }
                        }
                        Some(Message::Other) if !echo => {}
                        _ => out.write_all(&rewritten)?,
                    }
                    line.clear();
                }
                out.flush()?;
            }
        }

        let status = child.wait()?;
        drop(stdin);
        let _ = stderr_thread.join();
        if status.code() == Some(255) {
            bail!("lost connection to {}", conn.label());
        }
        Ok(Outcome {
            code: status.code().unwrap_or(1),
            artifacts,
        })
    }

    /// Copy files under the local root from their remote counterparts.
    pub fn fetch(&self, conn: &Conn, files: &[PathBuf]) -> Result<()> {
        let mut relative = BTreeSet::new();
        for file in files {
            match file
                .strip_prefix(&self.local_root)
                .ok()
                .and_then(Path::to_str)
            {
                Some(rel) => {
                    relative.insert(rel.to_owned());
                }
                None => ui::warn(format!(
                    "not fetching {}: outside the workspace (custom target dir?)",
                    file.display()
                )),
            }
        }
        if relative.is_empty() {
            return Ok(());
        }
        let count = relative.len();
        ui::status(
            "Fetching",
            format!("{count} artifact{}", if count == 1 { "" } else { "s" }),
        );
        self.pull(conn, &relative, &self.local_root)
            .with_context(|| format!("fetching artifacts from {}", conn.label()))
    }

    /// Copy files (relative to the root) from the remote copy into `dest`.
    fn pull(&self, conn: &Conn, relative: &BTreeSet<String>, dest: &Path) -> Result<()> {
        let mut child = conn
            .rsync()
            .args(["--archive", "--compress", "--from0", "--files-from=-"])
            .arg(conn.rsync_path(&format!("{}/", self.remote_root)))
            .arg(format!("{}/", dest.display()))
            .stdin(Stdio::piped())
            .spawn()
            .context("running rsync")?;
        let mut list = Vec::new();
        for rel in relative {
            list.extend_from_slice(rel.as_bytes());
            list.push(0);
        }
        child.stdin.take().unwrap().write_all(&list)?;
        let status = child.wait()?;
        if !status.success() {
            bail!("rsync failed ({status})");
        }
        Ok(())
    }

    /// The local check `cargo fix` makes before editing sources (the remote
    /// copy has no `.git`): refuse if they have uncommitted changes, unless
    /// the user allowed that.
    pub fn check_vcs(&self, args: &[String]) -> Result<()> {
        if cargo_args::has_flag(args, "--allow-no-vcs")
            || cargo_args::has_flag(args, "--allow-dirty")
        {
            return Ok(());
        }
        let sub = cargo_args::subcommand(args).unwrap_or("fix");
        let output = Command::new("git")
            .args(["status", "--porcelain", "--", "."])
            .current_dir(&self.local_root)
            .stderr(Stdio::null())
            .output();
        let Some(output) = output.ok().filter(|o| o.status.success()) else {
            bail!(
                "no git repository found for this workspace, and `cargo {sub}` can overwrite \
                 your files; commit them to git first, or pass `--allow-no-vcs`"
            );
        };
        let allow_staged = cargo_args::has_flag(args, "--allow-staged");
        let text = String::from_utf8_lossy(&output.stdout);
        let dirty: Vec<&str> = text
            .lines()
            // `XY path`: Y is the worktree; staged-only changes have a space there.
            .filter(|line| {
                !allow_staged || line.starts_with("??") || line.as_bytes().get(1) != Some(&b' ')
            })
            .map(|line| line.get(3..).unwrap_or(line))
            .collect();
        if !dirty.is_empty() {
            bail!(
                "the workspace has uncommitted changes that `cargo {sub}` could overwrite:\n  {}\n\
                 commit or stash them first, or pass `--allow-dirty` (`--allow-staged` for \
                 staged changes)",
                dirty.join("\n  ")
            );
        }
        Ok(())
    }

    /// Bring back files a command changed on the remote. Files changed locally
    /// since `synced_at` are kept; if the remote changed them too, say so.
    pub fn sync_back(&self, conn: &Conn, synced_at: SystemTime) -> Result<()> {
        let output = conn
            .rsync()
            .args(["--archive", "--dry-run", "--out-format=%i %n"])
            .args(SYNC_FILTERS)
            .arg(conn.rsync_path(&format!("{}/", self.remote_root)))
            .arg(format!("{}/", self.local_root_str))
            .stdin(Stdio::null())
            .output()
            .context("running rsync")?;
        if !output.status.success() {
            bail!(
                "checking {} for changed files failed: {}",
                conn.label(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let text = String::from_utf8_lossy(&output.stdout);
        // `>f` lines: regular files that would be transferred.
        let changed = text
            .lines()
            .filter_map(|line| line.strip_prefix(">f")?.split_once(' ').map(|(_, n)| n));

        let mut pull = BTreeSet::new();
        let mut edited_here = Vec::new();
        for rel in changed {
            let modified = std::fs::metadata(self.local_root.join(rel)).and_then(|m| m.modified());
            match modified {
                Ok(time) if time > synced_at => edited_here.push(rel.to_owned()),
                _ => {
                    pull.insert(rel.to_owned());
                }
            }
        }

        // The next sync overwrites the remote copy, so keep cargo's version
        // locally, where it's never synced.
        let conflicts: BTreeSet<String> = self
            .changed_remotely(conn, &edited_here, synced_at)?
            .into_iter()
            .collect();
        if !conflicts.is_empty() {
            let dir = self.local_root.join("target/cargo-plane/conflicts");
            std::fs::create_dir_all(&dir)?;
            self.pull(conn, &conflicts, &dir)
                .with_context(|| format!("copying cargo's versions from {}", conn.label()))?;
            for rel in &conflicts {
                let saved = dir.join(rel);
                let shown = saved.strip_prefix(&self.local_root).unwrap_or(&saved);
                ui::warn(format!(
                    "{rel} changed here while cargo ran, so it's kept as is; cargo's version \
                     is in {}",
                    shown.display()
                ));
            }
        }
        if pull.is_empty() {
            return Ok(());
        }
        self.pull(conn, &pull, &self.local_root)
            .with_context(|| format!("copying changed files back from {}", conn.label()))?;
        let names: Vec<&str> = pull.iter().map(String::as_str).collect();
        ui::status("Updated", names.join(", "));
        Ok(())
    }

    /// Which of `files` were modified on the remote after `synced_at`. Synced
    /// files keep their local times, so a newer one was written there.
    fn changed_remotely(
        &self,
        conn: &Conn,
        files: &[String],
        synced_at: SystemTime,
    ) -> Result<Vec<String>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let script = format!(
            "cd {} && for f in {}; do stat -c %Y -- \"$f\" 2>/dev/null || echo 0; done",
            shell_quote(&self.remote_root),
            shell_join(files)
        );
        let output = conn.shell(&script)?.stdin(Stdio::null()).output()?;
        let since = synced_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            // Allow for the clocks being a little apart.
            .saturating_sub(2);
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(files
            .iter()
            .zip(text.lines())
            .filter(|(_, mtime)| mtime.trim().parse::<u64>().is_ok_and(|t| t >= since))
            .map(|(file, _)| file.clone())
            .collect())
    }

    /// The remote counterpart of the current directory.
    pub fn remote_cwd(&self) -> String {
        match self.subdir.as_str() {
            "" => self.remote_root.clone(),
            sub => format!("{}/{sub}", self.remote_root),
        }
    }

    fn script(&self, conn: &Conn, program: &str, args: &[String]) -> Result<String> {
        let mut env: BTreeMap<String, String> = conn.env().clone();
        for var in FORWARDED_ENV {
            if let Ok(value) = std::env::var(var) {
                env.insert((*var).into(), value);
            }
        }
        let stderr = io::stderr();
        if stderr.is_terminal() {
            if !cargo_args::has_flag(args, "--color")
                && !env.contains_key("CARGO_TERM_COLOR")
                && ui::color()
            {
                env.insert("CARGO_TERM_COLOR".into(), "always".into());
            }
            if !cargo_args::is_quiet(args)
                && let Some((width, _)) = terminal_size::terminal_size_of(&stderr)
            {
                env.insert("CARGO_TERM_PROGRESS_WHEN".into(), "always".into());
                env.insert("CARGO_TERM_PROGRESS_WIDTH".into(), width.0.to_string());
            }
        }

        let mut script = env_prelude(conn.prelude(), &env)?;
        script += &format!("cd {} || exit 1\n", shell_quote(&self.remote_cwd()));
        if program == "cargo" {
            script += &add_missing_component(args);
        }
        // `setsid` puts cargo in its own process group, so the watcher can stop
        // cargo and everything it spawned once our stdin closes (the local
        // side was interrupted or lost its connection). SIGTERM, because
        // background jobs ignore SIGINT.
        // Background jobs get /dev/null as stdin, so hand the watcher ours via fd 3.
        script += "exec 3<&0\n";
        script += "s=; command -v setsid >/dev/null 2>&1 && s=setsid\n";
        script += &format!(
            "$s {} {} </dev/null &\n",
            shell_quote(program),
            shell_join(args)
        );
        script += "pid=$!\n";
        script += "$s sh -c 'cat >/dev/null; kill -TERM -$1 || kill -TERM $1' sh $pid <&3 >/dev/null 2>&1 &\n";
        script += "w=$!\n";
        script += "wait $pid\n";
        script += "rc=$?\n";
        script += "kill -TERM -$w 2>/dev/null || kill -TERM $w 2>/dev/null\n";
        script += "exit $rc\n";
        Ok(script)
    }
}

/// For `cargo clippy`: shell lines adding the rustup component if the
/// toolchain lacks it (rustup's minimal profile, the official Docker images).
/// After `cd`, so `rust-toolchain.toml` picks the toolchain.
fn add_missing_component(args: &[String]) -> String {
    let Some(sub) = cargo_args::subcommand(args) else {
        return String::new();
    };
    if sub != "clippy" {
        return String::new();
    }
    let (plus, toolchain) = match cargo_args::toolchain(args) {
        Some(t) => (format!("+{t} "), format!(" --toolchain {}", shell_quote(t))),
        None => (String::new(), String::new()),
    };
    format!(
        "if ! cargo {plus}{sub} --version >/dev/null 2>&1 && command -v rustup >/dev/null 2>&1; then \
         rustup component add{toolchain} clippy >&2; fi\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    #[test]
    fn adds_clippy_only_for_clippy() {
        assert_eq!(add_missing_component(&args("build")), "");
        assert_eq!(add_missing_component(&args("fmt")), "");
        assert_eq!(
            add_missing_component(&args("+nightly clippy")),
            "if ! cargo +nightly clippy --version >/dev/null 2>&1 && command -v rustup >/dev/null 2>&1; \
             then rustup component add --toolchain nightly clippy >&2; fi\n"
        );
    }
}
