//! Groups staticcheck findings by check: rtk asks for `-f json` (one object per finding) and
//! prints each check's count with a few example locations.

use crate::cmds::go::go_args::{bool_flag, flag_value, go_flags, wants_help};
use crate::cmds::go::go_run::append_hint;
use crate::cmds::go::go_tool::ToolBin;
use crate::core::arg_tokenizer::{TokenKind, ValueSpec};
use crate::core::args_utils;
use crate::core::runner;
use crate::core::tee;
use crate::core::truncate::{self, CAP_ERRORS, CAP_WARNINGS};
use anyhow::Result;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const TOOL: &str = "staticcheck";
const MAX_CHECKS: usize = CAP_ERRORS;
// One check can hold hundreds of findings (SA1019 ×362 on grpc-go); five show its pattern.
const MAX_LOCATIONS: usize = truncate::reduced(CAP_WARNINGS, 5);

#[derive(Debug, Deserialize)]
struct Finding {
    code: String,
    location: Location,
    message: String,
}

#[derive(Debug, Deserialize)]
struct Location {
    file: String,
    line: u32,
}

#[derive(Debug, Default)]
struct Report {
    findings: Vec<Finding>,
    unparsed: Vec<String>,
}

fn takes_value(_kind: TokenKind, name: &str) -> Option<ValueSpec> {
    matches!(
        name,
        "checks"
            | "explain"
            | "f"
            | "fail"
            | "go"
            | "tags"
            | "debug.cpuprofile"
            | "debug.memprofile"
            | "debug.measure-analyzers"
            | "debug.trace"
    )
    .then(ValueSpec::value)
}

/// Forms whose output is not a findings report, or whose format the user chose.
fn passes_through(args: &[String]) -> bool {
    let flags = go_flags(args, &takes_value);
    let t = &flags.tokens;
    wants_help(t)
        || flag_value(t, "f").is_some()
        || flag_value(t, "explain").is_some()
        || bool_flag(t, "version")
        || bool_flag(t, "debug.version")
        || bool_flag(t, "list-checks")
        || bool_flag(t, "matrix")
}

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let args = args_utils::restore_double_dash(args);
    run_with(ToolBin::Direct, &args, verbose)
}

/// Entry for both executables. `args` must already have `--` restored.
pub(crate) fn run_with(bin: ToolBin, args: &[String], verbose: u8) -> Result<i32> {
    if passes_through(args) {
        return bin.passthrough(TOOL, args, verbose);
    }
    let mut cmd = bin.command(TOOL);
    // JSON is the only format with one finding per line and a stable shape. Go's flag package
    // reads flags only before the first package argument, so it goes first.
    cmd.args(["-f", "json"]).args(args);
    let dirs = working_dirs(
        std::env::current_dir().ok(),
        std::env::var_os("PWD").map(PathBuf::from),
    );
    let tool_name = bin.tool_name(TOOL);
    if verbose > 0 {
        eprintln!("Running: {tool_name} -f json {}", args.join(" "));
    }
    runner::run_filtered_with_exit(
        cmd,
        &tool_name,
        &args.join(" "),
        move |output, exit_code| {
            let report = parse(output);
            if report.findings.is_empty() {
                if !output.trim().is_empty() {
                    eprintln!(
                        "rtk: filter warning: staticcheck output is not JSON, showing it unchanged"
                    );
                }
                return output.to_string();
            }
            let bases: Vec<&Path> = dirs.iter().map(PathBuf::as_path).collect();
            let filtered = render(&report, &bases, true);
            if verbose > 0 {
                eprintln!(
                    "rtk staticcheck: {} findings, {} lines out",
                    report.findings.len(),
                    filtered.lines().count()
                );
            }
            append_hint(output, filtered, exit_code, || {
                tee::force_tee_hint(&render(&report, &bases, false), TOOL)
            })
        },
        runner::RunOptions::stdout_only().tee(TOOL),
    )
}

fn parse(output: &str) -> Report {
    let mut report = Report::default();
    for line in output.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match serde_json::from_str::<Finding>(line) {
            Ok(finding) => report.findings.push(finding),
            Err(_) => report.unparsed.push(line.to_string()),
        }
    }
    report
}

fn render(report: &Report, bases: &[&Path], capped: bool) -> String {
    let mut groups: Vec<(&str, Vec<&Finding>)> = Vec::new();
    for finding in &report.findings {
        match groups.iter_mut().find(|(code, _)| *code == finding.code) {
            Some((_, members)) => members.push(finding),
            None => groups.push((finding.code.as_str(), vec![finding])),
        }
    }
    // A compile error usually explains the findings that follow it, so it leads.
    groups.sort_by(|(a, am), (b, bm)| {
        (*a != "compile")
            .cmp(&(*b != "compile"))
            .then(bm.len().cmp(&am.len()))
            .then(a.cmp(b))
    });
    let files: HashSet<&str> = report
        .findings
        .iter()
        .map(|f| f.location.file.as_str())
        .filter(|f| !f.is_empty())
        .collect();
    let mut out = vec![format!(
        "staticcheck: {} in {} ({})",
        plural(report.findings.len(), "finding"),
        plural(files.len(), "file"),
        plural(groups.len(), "check")
    )];
    let max_checks = if capped { MAX_CHECKS } else { usize::MAX };
    let max_locations = if capped { MAX_LOCATIONS } else { usize::MAX };
    for (code, members) in groups.iter().take(max_checks) {
        out.push(format!("{code} ({}x)", members.len()));
        for finding in members.iter().take(max_locations) {
            out.extend(finding_lines(finding, bases));
        }
        if members.len() > max_locations {
            out.push(format!("  … +{} more", members.len() - max_locations));
        }
    }
    if groups.len() > max_checks {
        out.push(format!("… +{} more checks", groups.len() - max_checks));
    }
    if !report.unparsed.is_empty() {
        out.push("unparsed:".to_string());
        out.extend(report.unparsed.iter().map(|l| format!("  {l}")));
    }
    out.join("\n")
}

fn finding_lines(finding: &Finding, bases: &[&Path]) -> Vec<String> {
    // Compile errors have no location; their message holds the compiler's own lines.
    if finding.location.file.is_empty() {
        return finding
            .message
            .lines()
            .map(|l| format!("  {}", l.trim_end()))
            .collect();
    }
    let first = finding.message.lines().next().unwrap_or("").trim_end();
    vec![format!(
        "  {}:{} {first}",
        relative(&finding.location.file, bases),
        finding.location.line
    )]
}

/// Every spelling of the working directory staticcheck may print paths under: `getcwd`
/// resolves symlinks (`/var` → `/private/var` on macOS) while Go's `os.Getwd` keeps `$PWD`.
fn working_dirs(cwd: Option<PathBuf>, pwd: Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = cwd.into_iter().collect();
    if let Some(pwd) = pwd.filter(|p| p.is_absolute())
        && !dirs.contains(&pwd)
    {
        dirs.push(pwd);
    }
    dirs
}

fn relative(file: &str, bases: &[&Path]) -> String {
    bases
        .iter()
        .find_map(|base| Path::new(file).strip_prefix(base).ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| file.to_string())
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmds::go::go_run::test_support::{assert_savings, s};

    const GRPC: &str = include_str!("../../../tests/fixtures/go_staticcheck_grpc_raw.jsonl");
    const COMPILE: &str = include_str!("../../../tests/fixtures/go_staticcheck_compile_raw.jsonl");
    const BASE: &str = "/tmp/rtk-fixture/grpc-go";

    #[test]
    fn filters_plain_runs() {
        assert!(!passes_through(&s(&["./..."])));
        assert!(!passes_through(&s(&["-checks", "all", "./..."])));
        assert!(!passes_through(&s(&["-tests=false", "./..."])));
        assert!(!passes_through(&s(&["-version=false", "./..."])));
    }

    #[test]
    fn a_users_format_and_informational_flags_pass_through() {
        for args in [
            &["-f", "text", "./..."][..],
            &["-f=stylish", "./..."],
            &["-version"],
            &["-list-checks"],
            &["-explain", "SA1019"],
            &["-matrix"],
            &["-h"],
        ] {
            assert!(passes_through(&s(args)), "{args:?}");
        }
    }

    #[test]
    fn a_flag_value_that_looks_like_f_is_not_a_format_flag() {
        // Go's flag package gives `-checks` the next argument whatever it looks like.
        assert!(!passes_through(&s(&["-checks", "-f", "./..."])));
    }

    #[test]
    fn groups_by_check_with_most_frequent_first() {
        let out = render(&parse(GRPC), &[Path::new(BASE)], true);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "staticcheck: 120 findings in 64 files (3 checks)");
        let headers: Vec<&str> = lines
            .iter()
            .copied()
            .filter(|l| !l.starts_with(' ') && !l.starts_with("staticcheck:"))
            .collect();
        assert_eq!(headers, ["SA1019 (70x)", "ST1019 (47x)", "SA4006 (3x)"]);
        assert!(out.contains("\n  … +65 more\n"), "{out}");
        assert!(out.contains("\n  … +42 more\n"), "{out}");
        assert!(out.ends_with(
            "SA4006 (3x)\n\
             \x20 binarylog/binarylog_end2end_test.go:678 this value of idInRPC is never used\n\
             \x20 binarylog/binarylog_end2end_test.go:681 this value of idInRPC is never used\n\
             \x20 binarylog/binarylog_end2end_test.go:747 this value of idInRPC is never used"
        ));
        assert_eq!(lines.len(), 19);
        assert!(
            lines[2].starts_with("  authz/rbac_translator.go:143 "),
            "{}",
            lines[2]
        );
        assert!(!lines[2].ends_with(' '));
    }

    #[test]
    fn uncapped_rendering_keeps_every_finding() {
        let out = render(&parse(GRPC), &[Path::new(BASE)], false);
        assert_eq!(out.lines().count(), 1 + 3 + 120);
        assert!(!out.contains("… +"));
    }

    #[test]
    fn compile_errors_lead_and_keep_their_message() {
        let mixed = format!("{GRPC}{COMPILE}");
        let out = render(&parse(&mixed), &[Path::new(BASE)], true);
        assert_eq!(out.lines().nth(1), Some("compile (1x)"), "{out}");
        assert_eq!(
            render(&parse(COMPILE), &[Path::new(BASE)], true),
            "staticcheck: 1 finding in 0 files (1 check)\n\
             compile (1x)\n\
             \x20 # scp\n\
             \x20 ./a.go:2:12: undefined: undefinedThing"
        );
    }

    #[test]
    fn paths_outside_base_stay_absolute() {
        let out = render(&parse(GRPC), &[Path::new("/somewhere/else")], true);
        assert!(out.contains("  /tmp/rtk-fixture/grpc-go/authz/rbac_translator.go:143 "));
    }

    #[test]
    fn both_spellings_of_the_working_directory_are_bases() {
        // macOS: getcwd resolves /var to /private/var, Go's os.Getwd keeps $PWD's /var.
        let dirs = working_dirs(
            Some(PathBuf::from("/private/var/x/grpc-go")),
            Some(PathBuf::from("/var/x/grpc-go")),
        );
        assert_eq!(
            dirs,
            [
                PathBuf::from("/private/var/x/grpc-go"),
                PathBuf::from("/var/x/grpc-go")
            ]
        );
        let same = working_dirs(Some(PathBuf::from("/a")), Some(PathBuf::from("/a")));
        assert_eq!(same, [PathBuf::from("/a")]);
        assert_eq!(
            working_dirs(None, Some(PathBuf::from("rel"))),
            Vec::<PathBuf>::new()
        );
        let bases: Vec<&Path> = [
            Path::new("/private/tmp/rtk-fixture/grpc-go"),
            Path::new(BASE),
        ]
        .to_vec();
        assert!(render(&parse(GRPC), &bases, true).contains("\n  authz/rbac_translator.go:143 "));
    }

    #[test]
    fn stray_lines_are_kept_as_unparsed() {
        let input = format!("{COMPILE}go: downloading example.com/x v1.0.0\n");
        let report = parse(&input);
        assert_eq!(report.unparsed, ["go: downloading example.com/x v1.0.0"]);
        assert!(
            render(&report, &[Path::new(BASE)], true)
                .ends_with("unparsed:\n  go: downloading example.com/x v1.0.0")
        );
    }

    #[test]
    fn non_json_output_yields_no_findings() {
        assert!(
            parse("-: error: flag provided but not defined\n")
                .findings
                .is_empty()
        );
        assert!(parse("").findings.is_empty());
    }

    #[test]
    fn savings_on_grpc() {
        assert_savings(
            "staticcheck grpc",
            GRPC,
            &render(&parse(GRPC), &[Path::new(BASE)], true),
        );
    }
}
