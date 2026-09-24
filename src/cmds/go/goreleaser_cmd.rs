//! Summarises `goreleaser release` and `build`: the outcome, what was built, and on failure
//! the step it stopped in with goreleaser's own error line.

use crate::cmds::go::go_run::append_hint;
use crate::cmds::go::go_tool::ToolBin;
use crate::core::arg_tokenizer::{self, Dialect, Token, TokenKind, ValueSpec};
use crate::core::args_utils;
use crate::core::runner;
use crate::core::tee;
use crate::core::truncate::CAP_LIST;
use crate::core::utils::strip_ansi;
use anyhow::Result;
use regex::Regex;
use std::sync::LazyLock;

const TOOL: &str = "goreleaser";
const MAX_ARTIFACTS: usize = CAP_LIST;

static FINAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(release|build) (succeeded|failed) after (\S+)").unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sub {
    Release,
    Build,
}

fn global_takes_value(_kind: TokenKind, _name: &str) -> Option<ValueSpec> {
    None
}

fn release_takes_value(kind: TokenKind, name: &str) -> Option<ValueSpec> {
    match (kind, name) {
        (TokenKind::Short, "f" | "p") => Some(ValueSpec::value()),
        (
            TokenKind::Long,
            "config"
            | "parallelism"
            | "release-footer"
            | "release-footer-tmpl"
            | "release-header"
            | "release-header-tmpl"
            | "release-notes"
            | "release-notes-tmpl"
            | "skip"
            | "timeout",
        ) => Some(ValueSpec::value()),
        _ => None,
    }
}

fn build_takes_value(kind: TokenKind, name: &str) -> Option<ValueSpec> {
    match (kind, name) {
        (TokenKind::Short, "f" | "p" | "o") => Some(ValueSpec::value()),
        (TokenKind::Long, "config" | "parallelism" | "id" | "output" | "skip" | "timeout") => {
            Some(ValueSpec::value())
        }
        _ => None,
    }
}

fn has_any(tokens: &[Token<'_>], flags: &[(&str, Option<&str>)]) -> bool {
    tokens.iter().any(|t| {
        flags.iter().any(|(long, short)| {
            (t.kind == TokenKind::Long && t.text == *long)
                || (t.kind == TokenKind::Short && Some(t.text) == *short)
        })
    })
}

/// `release`/`build` when rtk summarises them; `None` for everything left as goreleaser prints it.
fn classify(args: &[String]) -> Option<Sub> {
    let globals = arg_tokenizer::tokenize_grammar(args, &global_takes_value, Dialect::Posix);
    let sub_index = globals
        .iter()
        .find(|t| t.is_free_positional())?
        .source_index;
    let sub = match args[sub_index].as_str() {
        "release" => Sub::Release,
        "build" => Sub::Build,
        _ => return None,
    };
    let before: Vec<Token<'_>> = globals
        .iter()
        .copied()
        .filter(|t| t.source_index < sub_index)
        .collect();
    let chatty = [
        ("help", Some("h")),
        ("version", Some("v")),
        ("verbose", None),
        ("debug", None),
    ];
    if has_any(&before, &chatty) {
        return None;
    }
    let grammar: &dyn Fn(TokenKind, &str) -> Option<ValueSpec> = match sub {
        Sub::Release => &release_takes_value,
        Sub::Build => &build_takes_value,
    };
    let own = arg_tokenizer::tokenize_grammar(&args[sub_index + 1..], grammar, Dialect::Posix);
    if has_any(
        &own,
        &[("help", Some("h")), ("verbose", None), ("debug", None)],
    ) {
        return None;
    }
    Some(sub)
}

#[derive(Debug, Default)]
struct Log {
    last_step: Option<String>,
    targets: Vec<String>,
    archives: Vec<String>,
    checksums: bool,
    snapshot: bool,
    verdict: Option<(String, bool, String)>,
    errors: Vec<String>,
    warnings: Vec<String>,
}

/// A `key=value` field of a goreleaser log line; targets and dist paths hold no spaces.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.split_whitespace()
        .find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
}

fn parse(output: &str) -> Log {
    let mut log = Log::default();
    let plain = strip_ansi(output);
    // goreleaser prints a multi-line field (a compiler error) as indented lines under its `⨯`.
    let mut in_error = false;
    for line in plain.lines() {
        let trimmed = line.trim_start();
        let top_level = line.len() - trimmed.len() <= 2;
        let (failed_bullet, text) = if let Some(t) = trimmed.strip_prefix("• ") {
            (false, t)
        } else if let Some(t) = trimmed.strip_prefix("⨯ ") {
            (true, t)
        } else {
            if in_error && !trimmed.trim_end().is_empty() {
                log.errors.push(format!("  {}", trimmed.trim_end()));
            }
            continue;
        };
        in_error = failed_bullet;
        let trimmed = trimmed.trim_end();
        // goreleaser aligns `key=value` fields after the message with a run of spaces.
        let message = text.split("  ").next().unwrap_or(text).trim();
        if text.contains("level=warn") || message.starts_with("DEPRECATED") {
            log.warnings.push(trimmed.to_string());
        }
        if let Some(c) = FINAL_RE.captures(message) {
            log.verdict = Some((c[1].to_string(), &c[2] == "succeeded", c[3].to_string()));
            if failed_bullet {
                log.errors.push(trimmed.to_string());
            }
            continue;
        }
        if failed_bullet {
            log.errors.push(trimmed.to_string());
            continue;
        }
        if top_level {
            match message {
                "snapshotting" => log.snapshot = true,
                "calculating checksums" => log.checksums = true,
                _ => {}
            }
            if !message.starts_with("thanks for using") {
                log.last_step = Some(message.to_string());
            }
        } else if message == "building" {
            log.targets
                .extend(field(text, "target").map(str::to_string));
        } else if message == "archiving" {
            log.archives.extend(field(text, "name").map(str::to_string));
        }
    }
    log
}

fn count(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// Header plus item lines; `None` without goreleaser's final verdict line.
fn render(output: &str, capped: bool) -> Option<Vec<String>> {
    let log = parse(output);
    let (kind, succeeded, took) = log.verdict.clone()?;
    let mut out = Vec::new();
    if succeeded {
        let mut parts = Vec::new();
        if !log.targets.is_empty() {
            parts.push(count(log.targets.len(), "build"));
        }
        if !log.archives.is_empty() {
            parts.push(count(log.archives.len(), "archive"));
        }
        if log.checksums {
            parts.push("checksums".to_string());
        }
        let snapshot = if log.snapshot { " (snapshot)" } else { "" };
        let counts = if parts.is_empty() {
            String::new()
        } else {
            format!(" — {}", parts.join(", "))
        };
        out.push(format!(
            "goreleaser: {kind} succeeded after {took}{snapshot}{counts}"
        ));
        let items = if log.archives.is_empty() {
            &log.targets
        } else {
            &log.archives
        };
        let limit = if capped { MAX_ARTIFACTS } else { usize::MAX };
        out.extend(items.iter().take(limit).map(|i| format!("  {i}")));
    } else {
        let step = log
            .last_step
            .as_deref()
            .map(|s| format!(" in \"{s}\""))
            .unwrap_or_default();
        out.push(format!("goreleaser: {kind} failed after {took}{step}"));
        out.extend(log.errors.iter().map(|e| format!("  {e}")));
    }
    out.extend(log.warnings.iter().map(|w| format!("  {w}")));
    Some(out)
}

/// How many artifact lines a successful run lists (archives, or build targets without them).
fn item_count(output: &str) -> usize {
    let log = parse(output);
    if log.archives.is_empty() {
        log.targets.len()
    } else {
        log.archives.len()
    }
}

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let args = args_utils::restore_double_dash(args);
    run_with(ToolBin::Direct, &args, verbose)
}

/// Entry for both executables. `args` must already have `--` restored.
pub(crate) fn run_with(bin: ToolBin, args: &[String], verbose: u8) -> Result<i32> {
    if classify(args).is_none() {
        return bin.passthrough(TOOL, args, verbose);
    }
    let mut cmd = bin.command(TOOL);
    cmd.args(args);
    let tool_name = bin.tool_name(TOOL);
    if verbose > 0 {
        eprintln!("Running: {tool_name} {}", args.join(" "));
    }
    // goreleaser logs on stderr, so the combined stream is the report.
    runner::run_filtered_with_exit(
        cmd,
        &tool_name,
        &args.join(" "),
        move |output, exit_code| {
            let Some(full) = render(output, false) else {
                if !output.trim().is_empty() {
                    eprintln!(
                        "rtk: filter warning: goreleaser output has no final result line, showing it unchanged"
                    );
                }
                return output.to_string();
            };
            if verbose > 0 {
                eprintln!(
                    "rtk goreleaser: {} lines in, {} out",
                    output.lines().count(),
                    full.len()
                );
            }
            if exit_code == 0 && item_count(output) > MAX_ARTIFACTS {
                // Header + items is the stored blob, so the first hidden item is line 1 + MAX + 1.
                if let Some(hint) =
                    tee::force_tee_tail_hint(&full.join("\n"), TOOL, 1 + MAX_ARTIFACTS + 1)
                {
                    let mut shown = render(output, true).unwrap_or_default();
                    shown.push(hint);
                    return shown.join("\n");
                }
                return full.join("\n");
            }
            append_hint(output, full.join("\n"), exit_code, || {
                tee::force_tee_hint(output, TOOL)
            })
        },
        runner::RunOptions::with_tee(TOOL),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmds::go::go_run::test_support::{assert_savings, s};

    const RELEASE: &str = include_str!("../../../tests/fixtures/go_goreleaser_release_raw.txt");
    const BUILD: &str = include_str!("../../../tests/fixtures/go_goreleaser_build_raw.txt");
    const FAIL: &str = include_str!("../../../tests/fixtures/go_goreleaser_fail_raw.txt");

    fn joined(output: &str) -> String {
        render(output, true).expect("final line").join("\n")
    }

    #[test]
    fn only_release_and_build_are_filtered() {
        assert_eq!(
            classify(&s(&["release", "--snapshot", "--clean"])),
            Some(Sub::Release)
        );
        assert_eq!(
            classify(&s(&["build", "-f", "x.yaml", "--single-target"])),
            Some(Sub::Build)
        );
        for args in [
            &["check"][..],
            &["init"],
            &["release", "--verbose"],
            &["--verbose", "release"],
            &["release", "--debug"],
            &["release", "-h"],
            &["-v"],
            &[],
        ] {
            assert_eq!(classify(&s(args)), None, "{args:?}");
        }
    }

    #[test]
    fn release_success_lists_archives() {
        assert_eq!(
            joined(RELEASE),
            "goreleaser: release succeeded after 2s (snapshot) — 8 builds, 8 archives, checksums\n\
             \x20 dist/rel_Darwin_x86_64.tar.gz\n\
             \x20 dist/rel_Linux_arm64.tar.gz\n\
             \x20 dist/rel_Linux_i386.tar.gz\n\
             \x20 dist/rel_Windows_i386.zip\n\
             \x20 dist/rel_Windows_x86_64.zip\n\
             \x20 dist/rel_Darwin_arm64.tar.gz\n\
             \x20 dist/rel_Windows_arm64.zip\n\
             \x20 dist/rel_Linux_x86_64.tar.gz"
        );
    }

    #[test]
    fn build_success_lists_targets() {
        assert_eq!(
            joined(BUILD),
            "goreleaser: build succeeded after 0s (snapshot) — 8 builds\n\
             \x20 windows_amd64_v1\n\
             \x20 darwin_arm64_v8.0\n\
             \x20 linux_amd64_v1\n\
             \x20 windows_arm64_v8.0\n\
             \x20 darwin_amd64_v1\n\
             \x20 linux_386_sse2\n\
             \x20 windows_386_sse2\n\
             \x20 linux_arm64_v8.0"
        );
    }

    #[test]
    fn failure_names_the_step_and_keeps_the_error_line() {
        let error_line = FAIL
            .lines()
            .map(str::trim_start)
            .find(|l| l.starts_with('⨯'))
            .expect("fixture has an error line");
        assert_eq!(
            joined(FAIL),
            format!(
                "goreleaser: release failed after 0s in \"running before hooks\"\n  {error_line}"
            )
        );
        // The snapshot's informational `error=` on a `•` line is not an error.
        assert!(!joined(FAIL).contains("ignoring errors"));
    }

    #[test]
    fn multi_line_error_fields_are_kept() {
        let input =
            include_str!("../../../tests/fixtures/go_goreleaser_build_compile_fail_raw.txt");
        assert_eq!(
            joined(input),
            "goreleaser: build failed after 0s in \"building binaries\"\n\
             \x20 ⨯ build failed after 0s\n\
             \x20   error=\n\
             \x20   │ build failed: exit status 1: # example.com/rel\n\
             \x20   │ ./main.go:3:15: undefined: undefinedFoo\n\
             \x20   target=darwin_arm64_v8.0"
        );
    }

    #[test]
    fn warnings_are_kept() {
        let input = RELEASE.replace(
            "  • archives\n",
            "  • archives\n    • DEPRECATED: archives.format should not be used anymore\n",
        );
        assert_ne!(input, RELEASE, "fixture wording changed");
        assert!(
            joined(&input)
                .ends_with("\n  • DEPRECATED: archives.format should not be used anymore")
        );
    }

    #[test]
    fn item_count_ignores_warnings() {
        let input = RELEASE.replace(
            "  • archives\n",
            "  • archives\n    • DEPRECATED: archives.format should not be used anymore\n",
        );
        assert_eq!(item_count(&input), 8);
        assert_eq!(item_count(BUILD), 8);
    }

    #[test]
    fn ansi_coloured_lines_parse() {
        let coloured = RELEASE.replace(
            "  • release succeeded after 2s",
            "  \x1b[32m•\x1b[0m \x1b[1mrelease succeeded after 2s\x1b[0m",
        );
        assert_ne!(coloured, RELEASE, "fixture wording changed");
        assert_eq!(joined(&coloured), joined(RELEASE));
    }

    #[test]
    fn uncapped_rendering_is_the_recall_blob() {
        let many: String = (0..25)
            .map(|i| {
                format!("    • archiving                                      name=dist/a{i}.zip\n")
            })
            .collect();
        let input = RELEASE.replace("  • archives\n", &format!("  • archives\n{many}"));
        assert_eq!(render(&input, false).expect("final").len(), 1 + 8 + 25);
        assert_eq!(render(&input, true).expect("final").len(), 1 + 20);
    }

    #[test]
    fn no_final_line_is_not_rendered() {
        assert_eq!(
            render(
                "  • starting release\n  • cleaning distribution directory\n",
                true
            ),
            None
        );
        assert_eq!(render("", true), None);
    }

    #[test]
    fn savings() {
        assert_savings("goreleaser release", RELEASE, &joined(RELEASE));
        assert_savings("goreleaser build", BUILD, &joined(BUILD));
        assert_savings("goreleaser fail", FAIL, &joined(FAIL));
    }
}
