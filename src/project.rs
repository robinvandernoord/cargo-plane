//! The local workspace and its mirror on the remote: sync, run cargo, fetch.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use anyhow::{Context, Result, bail};

use crate::artifacts::{self, Artifact, Message};
use crate::cargo_args;
use crate::output::{replace, rewrite_stream};
use crate::remote::{Conn, env_prelude};
use crate::ui;
use crate::util::{self, shell_join, shell_quote};

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
            .args([
                "--archive",
                "--delete",
                "--compress",
                "--exclude=/.git/",
                "--exclude=/target/",
                "--filter=:- .gitignore",
            ])
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
        let args: Vec<String> = args
            .iter()
            .map(|a| a.replace(&self.local_root_str, &self.remote_root))
            .collect();
        let script = self.script(conn, &args)?;

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

        let mut child = conn
            .rsync()
            .args(["--archive", "--compress", "--from0", "--files-from=-"])
            .arg(conn.rsync_path(&format!("{}/", self.remote_root)))
            .arg(format!("{}/", self.local_root_str))
            .stdin(Stdio::piped())
            .spawn()
            .context("running rsync")?;
        let mut list = Vec::new();
        for rel in &relative {
            list.extend_from_slice(rel.as_bytes());
            list.push(0);
        }
        child.stdin.take().unwrap().write_all(&list)?;
        let status = child.wait()?;
        if !status.success() {
            bail!("fetching artifacts from {} failed ({status})", conn.label());
        }
        Ok(())
    }

    fn script(&self, conn: &Conn, args: &[String]) -> Result<String> {
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
        let cwd = match self.subdir.as_str() {
            "" => self.remote_root.clone(),
            sub => format!("{}/{sub}", self.remote_root),
        };
        script += &format!("cd {} || exit 1\n", shell_quote(&cwd));
        // `setsid` puts cargo in its own process group, so the watcher can stop
        // cargo and everything it spawned once our stdin closes (the local
        // side was interrupted or lost its connection). SIGTERM, because
        // background jobs ignore SIGINT.
        // Background jobs get /dev/null as stdin, so hand the watcher ours via fd 3.
        script += "exec 3<&0\n";
        script += "s=; command -v setsid >/dev/null 2>&1 && s=setsid\n";
        script += &format!("$s cargo {} </dev/null &\n", shell_join(args));
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
