//! Renders gotestsum runs with rtk's `go test` view: rtk has gotestsum write its test events to
//! a temporary `--jsonfile` and reads that instead of the formatted output.

use crate::cmds::go::go_args::bool_flag;
use crate::cmds::go::go_tool::ToolBin;
use crate::core::arg_tokenizer::{self, Dialect, Token, TokenKind, ValueSpec};
use crate::core::args_utils;
use crate::core::runner;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const TOOL: &str = "gotestsum";
const FILTERED_FORMATS: &[&str] = &["pkgname", "pkgname-and-test-fails", "dots", "dots-v2"];
static TEMP_PATH_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Passthrough,
    Filter {
        inject_at: usize,
        user_jsonfile: Option<String>,
    },
}

fn takes_value(kind: TokenKind, name: &str) -> Option<ValueSpec> {
    match (kind, name) {
        (TokenKind::Short, "f") => Some(ValueSpec::value()),
        (TokenKind::Long, "rerun-fails") => Some(ValueSpec::attached_only()),
        (
            TokenKind::Long,
            "format"
            | "format-icons"
            | "hide-summary"
            | "jsonfile"
            | "jsonfile-timing-events"
            | "junitfile"
            | "junitfile-project-name"
            | "junitfile-testcase-classname"
            | "junitfile-testsuite-name"
            | "max-fails"
            | "packages"
            | "post-run-command"
            | "rerun-fails-max-failures"
            | "rerun-fails-report",
        ) => Some(ValueSpec::value()),
        _ => None,
    }
}

/// `go test`'s value-taking flags, test flags also in their `-test.` form.
fn go_test_takes_value(_kind: TokenKind, name: &str) -> Option<ValueSpec> {
    let name = name.strip_prefix("test.").unwrap_or(name);
    matches!(
        name,
        "run"
            | "skip"
            | "bench"
            | "benchtime"
            | "blockprofile"
            | "blockprofilerate"
            | "count"
            | "coverprofile"
            | "cpu"
            | "cpuprofile"
            | "fuzz"
            | "fuzztime"
            | "fuzzminimizetime"
            | "list"
            | "memprofile"
            | "memprofilerate"
            | "mutexprofile"
            | "mutexprofilefraction"
            | "o"
            | "outputdir"
            | "parallel"
            | "shuffle"
            | "timeout"
            | "trace"
            | "covermode"
            | "coverpkg"
            | "exec"
            | "vet"
            | "C"
            | "p"
            | "asmflags"
            | "buildmode"
            | "compiler"
            | "gccgoflags"
            | "gcflags"
            | "installsuffix"
            | "ldflags"
            | "mod"
            | "modfile"
            | "overlay"
            | "pgo"
            | "pkgdir"
            | "tags"
            | "toolexec"
    )
    .then(ValueSpec::value)
}

fn last_flag<'t, 'a>(
    tokens: &'t [Token<'a>],
    long: &str,
    short: Option<&str>,
) -> Option<&'t Token<'a>> {
    tokens.iter().rev().find(|t| {
        (t.kind == TokenKind::Long && t.text == long)
            || (t.kind == TokenKind::Short && Some(t.text) == short)
    })
}

fn classify(args: &[String], env_format: Option<&str>) -> Plan {
    let tokens = arg_tokenizer::tokenize_grammar(args, &takes_value, Dialect::Posix);
    let own = arg_tokenizer::before_dashdash(&tokens);
    let interactive = [
        ("help", Some("h")),
        ("version", None),
        ("watch", None),
        ("rerun-fails", None),
        ("raw-command", None),
    ];
    if interactive
        .iter()
        .any(|(long, short)| last_flag(own, long, *short).is_some())
    {
        return Plan::Passthrough;
    }
    if own
        .iter()
        .find(|t| t.is_free_positional())
        .is_some_and(|t| t.text == "tool")
    {
        return Plan::Passthrough;
    }
    let format = last_flag(own, "format", Some("f"))
        .and_then(|t| t.value(&tokens))
        .or(env_format);
    if format.is_some_and(|f| !FILTERED_FORMATS.contains(&f)) {
        return Plan::Passthrough;
    }
    if let Some(dd) = arg_tokenizer::dashdash_index(&tokens)
        && go_test_verbose(&args[tokens[dd].source_index + 1..])
    {
        return Plan::Passthrough;
    }
    Plan::Filter {
        inject_at: arg_tokenizer::injection_point(&tokens, args.len()),
        user_jsonfile: last_flag(own, "jsonfile", None)
            .and_then(|t| t.value(&tokens))
            .map(str::to_string),
    }
}

/// Whether the `go test` arguments ask for verbose output. `go test` reads flags after the
/// packages too, so the whole region is classified, up to `-args`, which hands the rest to the
/// test binary.
fn go_test_verbose(test_args: &[String]) -> bool {
    let tokens = arg_tokenizer::tokenize_grammar(test_args, &go_test_takes_value, Dialect::Msbuild);
    let end = tokens
        .iter()
        .position(|t| t.kind == TokenKind::Long && t.text == "args")
        .unwrap_or(tokens.len());
    bool_flag(&tokens[..end], "v") || bool_flag(&tokens[..end], "test.v")
}

fn with_jsonfile(args: &[String], inject_at: usize, path: &Path) -> Vec<String> {
    let mut out = args.to_vec();
    out.splice(
        inject_at..inject_at,
        ["--jsonfile".to_string(), path.display().to_string()],
    );
    out
}

fn unique_temp_suffix() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let pid = std::process::id();
    let seq = TEMP_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}{:x}{:x}", ts, pid, seq)
}

fn temp_jsonfile() -> PathBuf {
    std::env::temp_dir().join(format!("rtk_gotestsum_{}.json", unique_temp_suffix()))
}

/// Removes the events file only when rtk created it: a `--jsonfile` the user passed is theirs.
fn cleanup_jsonfile(path: &Path, created_by_rtk: bool, verbose: u8) {
    if !created_by_rtk {
        return;
    }
    match std::fs::remove_file(path) {
        Ok(()) if verbose > 0 => eprintln!("rtk gotestsum: removed {}", path.display()),
        Ok(()) => {}
        Err(e) if verbose > 0 => {
            eprintln!("rtk gotestsum: could not remove {}: {e}", path.display())
        }
        Err(_) => {}
    }
}

type FileStamp = (SystemTime, u64);

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// The events this run wrote. A file unchanged since `before` holds an earlier run's events
/// (gotestsum exited before writing), so it is not read. rtk's own file is removed here, in the
/// filter: a relayed SIGTERM ends rtk before any cleanup after the runner returns.
fn take_events(path: &Path, created_by_rtk: bool, before: Option<FileStamp>) -> Option<String> {
    if before.is_some() && file_stamp(path) == before {
        return None;
    }
    let events = std::fs::read_to_string(path).ok()?;
    if created_by_rtk {
        let _ = std::fs::remove_file(path);
    }
    Some(events)
}

/// The `go test` view of a gotestsum events file; `None` when it holds no test events.
fn render_events(events: &str) -> Option<String> {
    let has_events = events.lines().any(|l| {
        serde_json::from_str::<serde_json::Value>(l.trim()).is_ok_and(|v| v.get("Action").is_some())
    });
    has_events.then(|| crate::go_cmd::filter_go_test_json(events))
}

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let args = args_utils::restore_double_dash(args);
    run_with(ToolBin::Direct, &args, verbose)
}

/// Entry for both executables. `args` must already have `--` restored.
pub(crate) fn run_with(bin: ToolBin, args: &[String], verbose: u8) -> Result<i32> {
    let env_format = std::env::var("GOTESTSUM_FORMAT").ok();
    let Plan::Filter {
        inject_at,
        user_jsonfile,
    } = classify(args, env_format.as_deref())
    else {
        return bin.passthrough(TOOL, args, verbose);
    };
    let (path, created_by_rtk) = match user_jsonfile {
        Some(p) => (PathBuf::from(p), false),
        None => (temp_jsonfile(), true),
    };
    let full_args = if created_by_rtk {
        with_jsonfile(args, inject_at, &path)
    } else {
        args.to_vec()
    };
    let mut cmd = bin.command(TOOL);
    cmd.args(&full_args);
    let tool_name = bin.tool_name(TOOL);
    if verbose > 0 {
        eprintln!("Running: {tool_name} {}", full_args.join(" "));
    }
    let before = file_stamp(&path);
    let read_path = path.clone();
    let result = runner::run_filtered(
        cmd,
        &tool_name,
        &args.join(" "),
        move |output| match take_events(&read_path, created_by_rtk, before)
            .as_deref()
            .and_then(render_events)
        {
            Some(rendered) => rendered,
            None => {
                eprintln!(
                    "rtk: filter warning: gotestsum wrote no test events, showing its output unchanged"
                );
                output.to_string()
            }
        },
        runner::RunOptions::with_tee(TOOL),
    );
    // A failed spawn never reaches the filter; remove a file it might have left.
    if path.exists() {
        cleanup_jsonfile(&path, created_by_rtk, verbose);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmds::go::go_run::test_support::{assert_savings, s};

    const PASS_EVENTS: &str =
        include_str!("../../../tests/fixtures/go_gotestsum_pass_events.jsonl");
    const PASS_STDOUT: &str = include_str!("../../../tests/fixtures/go_gotestsum_pass_stdout.txt");
    const FAIL_EVENTS: &str =
        include_str!("../../../tests/fixtures/go_gotestsum_fail_events.jsonl");

    fn filtered(args: &[&str]) -> Option<(usize, Option<String>)> {
        match classify(&s(args), None) {
            Plan::Filter {
                inject_at,
                user_jsonfile,
            } => Some((inject_at, user_jsonfile)),
            Plan::Passthrough => None,
        }
    }

    #[test]
    fn compact_formats_are_filtered_and_inject_before_dashdash() {
        assert_eq!(filtered(&[]), Some((0, None)));
        assert_eq!(filtered(&["--format", "dots"]), Some((2, None)));
        assert_eq!(
            filtered(&["-f", "pkgname-and-test-fails", "--", "./..."]),
            Some((2, None))
        );
        assert_eq!(filtered(&["--", "-run", "-v", "./..."]), Some((0, None)));
        assert_eq!(filtered(&["--", "-v=false", "./..."]), Some((0, None)));
    }

    #[test]
    fn a_users_jsonfile_is_read_not_injected() {
        assert_eq!(
            filtered(&["--jsonfile", "out.json", "--", "./..."]),
            Some((2, Some("out.json".into())))
        );
        assert_eq!(
            filtered(&["--jsonfile=out.json"]),
            Some((1, Some("out.json".into())))
        );
    }

    #[test]
    fn detailed_and_interactive_forms_pass_through() {
        for args in [
            &["-f", "standard-verbose"][..],
            &["--format=testname"],
            &["--format", "testdox"],
            &["-f", "github-actions"],
            &["-f", "standard-quiet"],
            &["--", "-v", "./..."],
            &["--", "./...", "-v=true"],
            &["--", "-test.v", "./..."],
            &["--raw-command", "--", "cat", "events.json"],
            &["--watch"],
            &["--rerun-fails", "--packages", "./..."],
            &["--rerun-fails=3"],
            &["tool", "slowest"],
            &["--version"],
            &["-h"],
        ] {
            assert_eq!(filtered(args), None, "{args:?}");
        }
    }

    #[test]
    fn gotestsum_format_env_counts_like_the_flag() {
        assert!(matches!(
            classify(&[], Some("standard-verbose")),
            Plan::Passthrough
        ));
        assert!(matches!(classify(&[], Some("dots")), Plan::Filter { .. }));
        assert!(matches!(
            classify(&s(&["-f", "dots"]), Some("testname")),
            Plan::Filter { .. }
        ));
    }

    #[test]
    fn verbosity_after_args_goes_to_the_test_binary() {
        assert!(!go_test_verbose(&s(&["./...", "-args", "-v"])));
        assert!(go_test_verbose(&s(&["./...", "-v", "-args", "x"])));
    }

    #[test]
    fn injection_keeps_user_args_after_the_flag() {
        let args = s(&["-f", "dots", "--", "./..."]);
        assert_eq!(
            with_jsonfile(&args, 2, Path::new("/tmp/x.json")),
            s(&["-f", "dots", "--jsonfile", "/tmp/x.json", "--", "./..."])
        );
    }

    #[test]
    fn temp_jsonfile_lives_in_the_temp_dir() {
        let a = temp_jsonfile();
        let b = temp_jsonfile();
        assert_ne!(a, b);
        assert_eq!(a.parent(), Some(std::env::temp_dir().as_path()));
        let name = a.file_name().and_then(|n| n.to_str()).expect("utf-8 name");
        assert!(
            name.starts_with("rtk_gotestsum_") && name.ends_with(".json"),
            "{name}"
        );
    }

    #[test]
    fn cleanup_removes_only_rtk_files() {
        let ours = temp_jsonfile();
        let theirs = temp_jsonfile();
        std::fs::write(&ours, "{}").expect("write");
        std::fs::write(&theirs, "{}").expect("write");
        cleanup_jsonfile(&ours, true, 0);
        cleanup_jsonfile(&theirs, false, 0);
        assert!(!ours.exists());
        assert!(theirs.exists());
        std::fs::remove_file(&theirs).expect("remove");
    }

    #[test]
    fn rtk_events_are_read_and_removed_at_once() {
        // Removed inside the filter: a relayed SIGTERM kills rtk before any later cleanup.
        let path = temp_jsonfile();
        std::fs::write(&path, "events").expect("write");
        assert_eq!(take_events(&path, true, None).as_deref(), Some("events"));
        assert!(!path.exists());
        assert_eq!(take_events(&path, true, None), None);
    }

    #[test]
    fn a_users_jsonfile_the_run_did_not_rewrite_is_not_read() {
        let path = temp_jsonfile();
        std::fs::write(&path, "old run").expect("write");
        let before = file_stamp(&path);
        assert_eq!(take_events(&path, false, before), None);
        std::fs::write(&path, "this run, longer").expect("write");
        assert_eq!(
            take_events(&path, false, before).as_deref(),
            Some("this run, longer")
        );
        assert!(path.exists(), "a user's file is never removed");
        std::fs::remove_file(&path).expect("remove");
    }

    #[test]
    fn events_render_as_the_go_test_view() {
        let pass = render_events(PASS_EVENTS).expect("events");
        assert!(pass.starts_with("Go test: 146 passed"), "{pass}");
        let fail = render_events(FAIL_EVENTS).expect("events");
        assert_eq!(fail, crate::go_cmd::filter_go_test_json(FAIL_EVENTS));
        assert!(
            fail.starts_with("Go test: 5 passed, 2 failed in 2 packages"),
            "{fail}"
        );
        assert!(fail.contains("  [FAIL] TestBad\n"), "{fail}");
    }

    #[test]
    fn non_event_files_are_not_rendered() {
        assert_eq!(render_events(""), None);
        assert_eq!(render_events("not json\n{\"a\":1}\n"), None);
    }

    #[test]
    fn savings_on_passing_run() {
        assert_savings(
            "gotestsum pass",
            PASS_STDOUT,
            &render_events(PASS_EVENTS).expect("events"),
        );
    }
}
