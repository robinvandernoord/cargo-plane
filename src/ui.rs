//! Cargo-style status output on stderr.

use std::fmt::Display;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);

pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

pub fn color() -> bool {
    match std::env::var("CARGO_TERM_COLOR").as_deref() {
        Ok("always") => true,
        Ok("never") => false,
        _ => std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal(),
    }
}

/// `     Syncing foo to bar`, like cargo's `   Compiling foo`.
pub fn status(verb: &str, msg: impl Display) {
    if QUIET.load(Ordering::Relaxed) {
        return;
    }
    if color() {
        eprintln!("\x1b[1;32m{verb:>12}\x1b[0m {msg}");
    } else {
        eprintln!("{verb:>12} {msg}");
    }
}

pub fn warn(msg: impl Display) {
    label("warning", "33", msg);
}

pub fn error(msg: impl Display) {
    label("error", "31", msg);
}

fn label(label: &str, ansi: &str, msg: impl Display) {
    if color() {
        eprintln!("\x1b[1;{ansi}m{label}\x1b[0m\x1b[1m:\x1b[0m {msg}");
    } else {
        eprintln!("{label}: {msg}");
    }
}

/// Whether we can ask the user questions.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Ask for a line of input; an empty answer picks `default`.
pub fn ask(question: &str, default: Option<&str>) -> anyhow::Result<String> {
    loop {
        match default {
            Some("") | None => eprint!("{question}: "),
            Some(default) => eprint!("{question} [{default}]: "),
        }
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            anyhow::bail!("aborted");
        }
        match (line.trim(), default) {
            ("", Some(default)) => return Ok(default.to_owned()),
            ("", None) => {}
            (answer, _) => return Ok(answer.to_owned()),
        }
    }
}

pub fn confirm(question: &str, default: bool) -> anyhow::Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    loop {
        let answer = ask(&format!("{question} [{hint}]"), Some(""))?;
        match answer.to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => {}
        }
    }
}
