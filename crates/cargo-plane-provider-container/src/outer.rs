//! The SSH host that runs the container engine.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};

use crate::check_engine;

pub struct Outer {
    host: String,
    user: Option<String>,
    port: Option<u16>,
    engine: String,
}

impl Outer {
    pub fn new(host: &str, user: Option<&str>, port: Option<u16>, engine: &str) -> Result<Self> {
        if host.is_empty() || host.starts_with('-') || !proxy_safe(host) {
            bail!("invalid SSH host `{host}`");
        }
        if let Some(user) = user
            && (user.is_empty() || user.starts_with('-') || !proxy_safe(user))
        {
            bail!("invalid SSH user `{user}`");
        }
        check_engine(engine)?;
        Ok(Self {
            host: host.to_owned(),
            user: user.map(str::to_owned),
            port,
            engine: engine.to_owned(),
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// `ssh` arguments up to and including the destination. BatchMode makes
    /// a missing key or unknown host key fail instead of prompting with no
    /// terminal; `interactive` allows prompts, for setup's first connection.
    fn ssh_args(&self, interactive: bool) -> Vec<String> {
        let mut args = vec!["-T".to_owned()];
        if !interactive {
            args.extend(["-o".into(), "BatchMode=yes".into()]);
        }
        if let Some(port) = self.port {
            args.extend(["-p".into(), port.to_string()]);
        }
        if let Some(user) = &self.user {
            args.extend(["-l".into(), user.clone()]);
        }
        args.extend(["--".into(), self.host.clone()]);
        args
    }

    /// The engine command line for the host's shell.
    pub fn engine_cmd(&self, args: &[&str]) -> String {
        std::iter::once(self.engine.as_str())
            .chain(args.iter().copied())
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// A ProxyCommand running `script` on the host.
    ///
    /// cargo-plane hands it to rsync in `-e`, which splits that string itself
    /// and doesn't understand the shell's `'\''` escape, so words with spaces
    /// get double quotes and nothing needs escaping inside them (see
    /// `proxy_safe`). ssh expands `%` tokens in ProxyCommand, so literal ones
    /// are doubled.
    pub fn proxy_command(&self, script: &str) -> String {
        assert!(
            proxy_safe(script),
            "unquotable ProxyCommand script: {script}"
        );
        std::iter::once("ssh".to_owned())
            .chain(self.ssh_args(false))
            .chain([script.to_owned()])
            .map(|arg| {
                if arg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-./=:,+@%".contains(&b))
                {
                    arg
                } else {
                    format!("\"{arg}\"")
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
            .replace('%', "%%")
    }

    /// ssh running `script` with `sh`, so scripts are POSIX whatever the
    /// login shell is.
    fn command(&self, script: &str, interactive: bool) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.args(self.ssh_args(interactive))
            .arg(format!("sh -c {}", shell_quote(script)));
        cmd
    }

    /// Run `script` on the host, returning what it did whether it failed or not.
    fn output(&self, script: &str) -> Result<Output> {
        self.command(script, false)
            .stdin(Stdio::null())
            .output()
            .context("can't run ssh")
    }

    /// Run `script` on the host and return its stdout.
    pub fn run(&self, script: &str) -> Result<String> {
        let output = self.output(script)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Not the script: some are long, and stderr says what went wrong.
            bail!(
                "on {}: command failed ({}): {}",
                self.host,
                output.status,
                stderr.trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Run `script` on the host with its output on the terminal, feeding it
    /// `stdin`. With `interactive`, ssh may ask about host keys or passwords.
    pub fn run_visible(&self, script: &str, stdin: Option<&str>, interactive: bool) -> Result<()> {
        let mut cmd = self.command(script, interactive);
        // The plugin's stdout is cargo-plane's; keep it for results.
        cmd.stdout(std::io::stderr());
        if stdin.is_some() {
            cmd.stdin(Stdio::piped());
        } else if !interactive {
            cmd.stdin(Stdio::null());
        }
        let mut child = cmd.spawn().context("can't run ssh")?;
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().unwrap();
            pipe.write_all(input.as_bytes())?;
        }
        let status = child.wait()?;
        if !status.success() {
            bail!("on {}: command failed ({status})", self.host);
        }
        Ok(())
    }

    /// `user@host:port`, as far as configured: which SSH destination this is.
    pub fn destination(&self) -> String {
        let mut dest = self.host.clone();
        if let Some(user) = &self.user {
            dest = format!("{user}@{dest}");
        }
        if let Some(port) = self.port {
            dest = format!("{dest}:{port}");
        }
        dest
    }

    /// Fail with a hint if the engine is missing or unusable (no daemon, no
    /// permission), so callers don't mistake that for missing resources.
    pub fn check_access(&self) -> Result<()> {
        let script = format!("{} >/dev/null", self.engine_cmd(&["info"]));
        self.run(&script).map(drop).with_context(|| {
            if self.engine == "docker" {
                format!(
                    "can't use docker on {}; is it installed and the daemon running, and is \
                     your user in the `docker` group?",
                    self.host
                )
            } else {
                format!("can't use podman on {}; is it installed?", self.host)
            }
        })
    }

    /// Run `engine <args>` on the host: `Some(stdout)` if it worked, `None` if
    /// the engine says the object doesn't exist, an error otherwise (engine
    /// missing, daemon down, no permission, ...).
    fn engine_query(&self, args: &[&str]) -> Result<Option<String>> {
        let output = self.output(&self.engine_cmd(args))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() {
            return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
        }
        // ssh's own failures (connection, proxy, auth) exit with 255; their
        // messages can look like an engine's ("No such file or directory").
        if output.status.code() == Some(255) {
            bail!("can't SSH to {}: {}", self.host, stderr.trim());
        }
        if output.status.code() == Some(127) {
            bail!("{} is not installed on {}", self.engine, self.host);
        }
        if is_missing(&stderr) {
            return Ok(None);
        }
        bail!(
            "on {}: `{} {}` failed ({}): {}",
            self.host,
            self.engine,
            args.join(" "),
            output.status,
            stderr.trim()
        )
    }

    /// A volume's labels, or `None` if the volume doesn't exist.
    pub fn volume_labels(&self, volume: &str) -> Result<Option<HashMap<String, String>>> {
        let Some(output) =
            self.engine_query(&["volume", "inspect", "--format", "{{json .Labels}}", volume])?
        else {
            return Ok(None);
        };
        // `null` for a volume without labels.
        let labels: Option<HashMap<String, String>> = serde_json::from_str(output.trim())
            .with_context(|| format!("unexpected labels on volume {volume}: {output}"))?;
        Ok(Some(labels.unwrap_or_default()))
    }

    /// Whether the engine has a `kind` (container, volume, image) called `name`.
    pub fn exists(&self, kind: &str, name: &str) -> Result<bool> {
        Ok(self.engine_query(&[kind, "inspect", name])?.is_some())
    }

    /// The container's state (`running`, `exited`, ...) and the value of its
    /// `label`, or `None` if it doesn't exist.
    pub fn inspect(&self, container: &str, label: &str) -> Result<Option<Inspected>> {
        let format = format!("{{{{.State.Status}}}} {{{{index .Config.Labels {label:?}}}}}");
        let Some(output) =
            self.engine_query(&["container", "inspect", "--format", &format, container])?
        else {
            return Ok(None);
        };
        let output = output.trim();
        let (state, value) = output.split_once(' ').unwrap_or((output, ""));
        Ok(Some(Inspected {
            state: state.to_owned(),
            // What Go templates print for a missing key.
            label: Some(value)
                .filter(|v| !v.is_empty() && *v != "<no value>")
                .map(str::to_owned),
        }))
    }
}

/// Whether an engine error says the object doesn't exist. Docker: "No such
/// container: x", "no such volume", "No such image: x", "No such object";
/// Podman: "no such container", "no such volume", "x: image not known".
fn is_missing(stderr: &str) -> bool {
    let stderr = stderr.to_lowercase();
    [
        "no such container",
        "no such volume",
        "no such image",
        "no such object",
        "image not known",
    ]
    .iter()
    .any(|phrase| stderr.contains(phrase))
}

pub struct Inspected {
    pub state: String,
    pub label: Option<String>,
}

/// Whether `s` can go in double quotes in a ProxyCommand, for both the shell
/// and rsync: no quotes, backslashes or expansions, and no control characters.
fn proxy_safe(s: &str) -> bool {
    !s.chars()
        .any(|c| c.is_control() || matches!(c, '\'' | '"' | '\\' | '$' | '`' | '!'))
}

pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-./=:,+@%".contains(&b));
    if safe {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes() {
        assert_eq!(shell_quote("docker"), "docker");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn proxy_command_escapes_percent() {
        let outer = Outer::new("h%x", None, None, "podman").unwrap();
        assert_eq!(
            outer.proxy_command("true"),
            "ssh -T -o BatchMode=yes -- h%%x true"
        );
    }

    #[test]
    fn recognizes_missing_objects() {
        assert!(is_missing(
            "Error response from daemon: No such container: x"
        ));
        assert!(is_missing(
            "Error response from daemon: get x: no such volume"
        ));
        assert!(is_missing("Error: x: image not known"));
        assert!(!is_missing(
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock"
        ));
        assert!(!is_missing("permission denied while trying to connect"));
        assert!(!is_missing(
            "/bin/sh: 1: exec: cloudflared: No such file or directory"
        ));
    }

    #[test]
    fn scripts_run_in_sh() {
        let outer = Outer::new("h", None, None, "docker").unwrap();
        let cmd = outer.command("a && b's", false);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args.last().unwrap(), &r"sh -c 'a && b'\''s'");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Outer::new("", None, None, "docker").is_err());
        assert!(Outer::new("-oProxyCommand=x", None, None, "docker").is_err());
        assert!(Outer::new("h", None, None, "nerdctl").is_err());
        assert!(Outer::new("h'x", None, None, "docker").is_err());
        assert!(Outer::new("h", Some("a\"b"), None, "docker").is_err());
    }
}
