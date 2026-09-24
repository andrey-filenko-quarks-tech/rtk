//! Filters `go generate`: success collapses to `ok`, a failure keeps the failing generator's
//! last output lines. Go's own `running "…"` error lines arrive on stderr, forwarded whole.

use crate::cmds::go::go_mod_cmd::{self, append_hint, go_flags, has_flag, wants_help};
use crate::core::arg_tokenizer::{TokenKind, ValueSpec};
use crate::core::runner;
use crate::core::tee;
use crate::core::truncate::CAP_ERRORS;
use crate::core::utils::resolved_command;
use anyhow::Result;

const MAX_FAILURE_LINES: usize = CAP_ERRORS;
const GENERATE_TEE_LABEL: &str = "go-generate";

/// `go generate` plus `go help build`'s value-taking flags (go 1.27.1).
fn generate_takes_value(kind: TokenKind, name: &str) -> Option<ValueSpec> {
    if kind != TokenKind::Long {
        return None;
    }
    match name {
        "buildvcs" => Some(ValueSpec::attached_only()),
        "C" | "run" | "skip" | "p" | "covermode" | "coverpkg" | "asmflags" | "buildmode"
        | "compiler" | "gccgoflags" | "gcflags" | "installsuffix" | "ldflags" | "mod"
        | "modfile" | "overlay" | "pgo" | "pkgdir" | "tags" | "toolexec" => {
            Some(ValueSpec::value())
        }
        _ => None,
    }
}

/// `-n`/`-x`/`-v` ask for Go's own trace, which is theirs to read.
fn filters(args: &[String]) -> bool {
    let tokens = go_flags(args, &generate_takes_value).tokens;
    !(wants_help(&tokens) || ["n", "x", "v"].iter().any(|f| has_flag(&tokens, f)))
}

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let args = crate::core::args_utils::restore_double_dash(args);
    if !filters(&args) {
        return go_mod_cmd::run_go_passthrough("generate", &args, verbose);
    }
    let mut cmd = resolved_command("go");
    cmd.arg("generate").args(&args);
    if verbose > 0 {
        eprintln!("Running: go generate {}", args.join(" "));
    }
    runner::run_filtered_with_exit(
        cmd,
        "go generate",
        &args.join(" "),
        move |stdout, exit_code| {
            if verbose > 1 {
                eprintln!("{stdout}");
            }
            let filtered = filter_go_generate(stdout, exit_code);
            append_hint(stdout, filtered, exit_code, || {
                tee::force_tee_hint(stdout, GENERATE_TEE_LABEL)
            })
        },
        runner::RunOptions::stdout_only().tee(GENERATE_TEE_LABEL),
    )
}

/// stdout only: on success generators' chatter collapses to `ok`; on failure the failing
/// generator ran last, so its output is the tail.
fn filter_go_generate(stdout: &str, exit_code: i32) -> String {
    if exit_code == 0 {
        return "go generate: ok".to_string();
    }
    let lines: Vec<&str> = stdout.lines().collect();
    if lines.len() <= MAX_FAILURE_LINES {
        return stdout.to_string();
    }
    let hidden = lines.len() - MAX_FAILURE_LINES;
    let mut out = vec![format!("… ({hidden} earlier lines)")];
    out.extend(lines[hidden..].iter().map(|l| l.to_string()));
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmds::go::go_mod_cmd::tests::{assert_savings, s};

    #[test]
    fn classifies_generate() {
        assert!(filters(&s(&["./..."])));
        assert!(filters(&s(&["-run", "mockgen", "./..."])));
        for args in [&["-x", "./..."][..], &["-n"], &["-v", "./..."], &["-help"]] {
            assert!(!filters(&s(args)), "{args:?}");
        }
    }

    #[test]
    fn success_collapses_to_ok() {
        assert_eq!(filter_go_generate("", 0), "go generate: ok");
        assert_eq!(filter_go_generate("wrote a.go\n", 0), "go generate: ok");
    }

    #[test]
    fn failure_keeps_the_last_lines() {
        let raw: String = (1..=25).map(|i| format!("line {i}\n")).collect();
        let out = filter_go_generate(&raw, 1);
        assert!(out.starts_with("… (5 earlier lines)\nline 6\n"), "{out}");
        assert!(out.ends_with("line 25"), "{out}");
        assert_eq!(filter_go_generate("short\n", 1), "short\n");
    }

    #[test]
    fn failure_fixture() {
        let input = include_str!("../../../tests/fixtures/go_generate_fail_stdout_raw.txt");
        let out = filter_go_generate(input, 1);
        assert!(out.ends_with("generated_file_60"), "{out}");
        assert_savings("go generate (failure)", input, &out);
    }
}
