//! Decide what a `cargo plane <args>` invocation means.

/// Options before the subcommand that take a separate value.
const GLOBAL_OPTS_WITH_VALUE: &[&str] = &["--config", "-Z", "-C", "--color"];

#[derive(Debug, PartialEq)]
pub enum Invocation {
    /// Run as-is on the remote.
    Passthrough(Vec<String>),
    /// `cargo build`: run remotely, then fetch binaries and libraries.
    Build(CargoCall),
    /// `cargo run`: build remotely, fetch the binary, run it locally.
    Run {
        build: CargoCall,
        program_args: Vec<String>,
        bin: Option<Selector>,
    },
    /// `cargo test --local-run`: build test binaries remotely, run them locally.
    TestLocal {
        build: CargoCall,
        test_args: Vec<String>,
        no_fail_fast: bool,
    },
}

#[derive(Debug, PartialEq)]
pub struct CargoCall {
    pub args: Vec<String>,
    /// The user asked for JSON output themselves, so echo it.
    pub user_json: bool,
}

#[derive(Debug, PartialEq)]
pub enum Selector {
    Bin(String),
    Example(String),
}

pub fn parse(args: Vec<String>) -> Invocation {
    let Some(sub) = subcommand_index(&args) else {
        return Invocation::Passthrough(args);
    };
    let (global, rest) = args.split_at(sub);
    let (sub, rest) = (rest[0].as_str(), &rest[1..]);
    let (cargo_rest, after_dashes) = match rest.iter().position(|a| a == "--") {
        Some(i) => (&rest[..i], rest[i + 1..].to_vec()),
        None => (rest, Vec::new()),
    };

    match sub {
        "build" | "b" => Invocation::Build(cargo_call(global, "build", cargo_rest, &[])),
        "run" | "r" => Invocation::Run {
            bin: selector(cargo_rest),
            build: cargo_call(global, "build", cargo_rest, &[]),
            program_args: after_dashes,
        },
        "test" | "t" if cargo_rest.iter().any(|a| a == "--local-run") => {
            let cargo_rest: Vec<_> = cargo_rest
                .iter()
                .filter(|a| *a != "--local-run")
                .cloned()
                .collect();
            Invocation::TestLocal {
                no_fail_fast: cargo_rest.iter().any(|a| a == "--no-fail-fast"),
                build: cargo_call(global, "test", &cargo_rest, &["--no-run"]),
                test_args: after_dashes,
            }
        }
        _ => Invocation::Passthrough(args),
    }
}

pub fn is_quiet(args: &[String]) -> bool {
    args.iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "-q" || a == "--quiet")
}

pub fn has_flag(args: &[String], flag: &str) -> bool {
    let prefix = format!("{flag}=");
    args.iter()
        .take_while(|a| *a != "--")
        .any(|a| a == flag || a.starts_with(&prefix))
}

/// Value of `--flag value` or `--flag=value`, before any `--`.
pub fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let prefix = format!("{flag}=");
    let mut iter = args.iter().take_while(|a| *a != "--");
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter.next().map(String::as_str);
        }
        if let Some(value) = arg.strip_prefix(&prefix) {
            return Some(value);
        }
    }
    None
}

/// The cargo subcommand (`build`, `clippy`, ...), if any.
pub fn subcommand(args: &[String]) -> Option<&str> {
    subcommand_index(args).map(|i| args[i].as_str())
}

/// The toolchain picked with `+toolchain` before the subcommand, if any.
pub fn toolchain(args: &[String]) -> Option<&str> {
    let end = subcommand_index(args).unwrap_or(args.len());
    args[..end].iter().find_map(|a| a.strip_prefix('+'))
}

fn subcommand_index(args: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if GLOBAL_OPTS_WITH_VALUE.contains(&arg.as_str()) {
            i += 2;
        } else if arg.starts_with('-') || arg.starts_with('+') {
            i += 1;
        } else {
            return Some(i);
        }
    }
    None
}

fn selector(args: &[String]) -> Option<Selector> {
    let args = args.to_vec();
    flag_value(&args, "--bin")
        .map(|b| Selector::Bin(b.into()))
        .or_else(|| flag_value(&args, "--example").map(|e| Selector::Example(e.into())))
}

/// `<global> <sub> <extra> <rest>` plus a JSON message format we can parse.
fn cargo_call(global: &[String], sub: &str, rest: &[String], extra: &[&str]) -> CargoCall {
    let format = flag_value(rest, "--message-format");
    let user_json = format.is_some_and(|f| f.starts_with("json"));

    let mut args: Vec<String> = global.to_vec();
    args.push(sub.into());
    args.extend(extra.iter().map(|s| s.to_string()));
    let mut skip_value = false;
    for arg in rest {
        if skip_value {
            skip_value = false;
            continue;
        }
        // Human/short formats are replaced by ours; JSON ones are kept.
        if !user_json && arg == "--message-format" {
            skip_value = true;
            continue;
        }
        if !user_json && arg.starts_with("--message-format=") {
            continue;
        }
        args.push(arg.clone());
    }
    if !user_json {
        args.push("--message-format=json-render-diagnostics".into());
    }
    CargoCall { args, user_json }
}

#[cfg(test)]
mod tests {

    #[test]
    fn finds_subcommand_and_toolchain() {
        let args = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(
            subcommand(&args("+nightly -v clippy --fix")),
            Some("clippy")
        );
        assert_eq!(
            toolchain(&args("+nightly -v clippy --fix")),
            Some("nightly")
        );
        assert_eq!(toolchain(&args("build +x")), None);
        assert_eq!(subcommand(&args("--color always fmt")), Some("fmt"));
    }
    use super::*;

    fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn check_is_passthrough() {
        let args = s(&["check", "--all-targets"]);
        assert_eq!(parse(args.clone()), Invocation::Passthrough(args));
    }

    #[test]
    fn plain_test_is_passthrough() {
        let args = s(&["test", "-p", "foo", "--", "--nocapture"]);
        assert_eq!(parse(args.clone()), Invocation::Passthrough(args));
    }

    #[test]
    fn build_gets_json_format() {
        let Invocation::Build(call) = parse(s(&["+nightly", "-v", "build", "-r"])) else {
            panic!()
        };
        assert_eq!(
            call.args,
            s(&[
                "+nightly",
                "-v",
                "build",
                "-r",
                "--message-format=json-render-diagnostics"
            ])
        );
        assert!(!call.user_json);
    }

    #[test]
    fn user_json_is_kept() {
        let Invocation::Build(call) = parse(s(&["build", "--message-format", "json"])) else {
            panic!()
        };
        assert_eq!(call.args, s(&["build", "--message-format", "json"]));
        assert!(call.user_json);
    }

    #[test]
    fn human_format_is_replaced() {
        let Invocation::Build(call) = parse(s(&["build", "--message-format", "short"])) else {
            panic!()
        };
        assert_eq!(
            call.args,
            s(&["build", "--message-format=json-render-diagnostics"])
        );
    }

    #[test]
    fn run_splits_program_args() {
        let parsed = parse(s(&[
            "--config", "x=1", "run", "--bin", "app", "--", "-v", "--bin",
        ]));
        let Invocation::Run {
            build,
            program_args,
            bin,
        } = parsed
        else {
            panic!()
        };
        assert_eq!(
            build.args,
            s(&[
                "--config",
                "x=1",
                "build",
                "--bin",
                "app",
                "--message-format=json-render-diagnostics"
            ])
        );
        assert_eq!(program_args, s(&["-v", "--bin"]));
        assert_eq!(bin, Some(Selector::Bin("app".into())));
    }

    #[test]
    fn test_local_run() {
        let parsed = parse(s(&["t", "--local-run", "--no-fail-fast", "--", "filter"]));
        let Invocation::TestLocal {
            build,
            test_args,
            no_fail_fast,
        } = parsed
        else {
            panic!()
        };
        assert_eq!(
            build.args,
            s(&[
                "test",
                "--no-run",
                "--no-fail-fast",
                "--message-format=json-render-diagnostics"
            ])
        );
        assert_eq!(test_args, s(&["filter"]));
        assert!(no_fail_fast);
    }

    #[test]
    fn flags_after_dashes_are_ignored() {
        let args = s(&["run", "--", "-q"]);
        assert!(!is_quiet(&args));
        assert!(is_quiet(&s(&["build", "-q"])));
        assert_eq!(flag_value(&s(&["run", "--", "--bin", "x"]), "--bin"), None);
    }
}
