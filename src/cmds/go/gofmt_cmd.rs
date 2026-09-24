//! `gofmt`/`goimports` `-l` keeps the tool's own file list, capped; `-d` becomes one
//! `path (+a -r)` line per file.

use crate::cmds::go::go_args::{GoFlags, bool_flag, go_flags, wants_help};
use crate::cmds::go::go_run::append_hint;
use crate::core::arg_tokenizer::ValueSpec;
use crate::core::args_utils;
use crate::core::runner;
use crate::core::tee;
use crate::core::truncate::CAP_LIST;
use crate::core::utils::resolved_command;
use anyhow::Result;
use std::ffi::OsString;

const MAX_FILES: usize = CAP_LIST;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FmtTool {
    Gofmt,
    Goimports,
}

impl FmtTool {
    fn name(self) -> &'static str {
        match self {
            FmtTool::Gofmt => "gofmt",
            FmtTool::Goimports => "goimports",
        }
    }

    fn takes_value(self, name: &str) -> bool {
        match self {
            FmtTool::Gofmt => matches!(name, "cpuprofile" | "r"),
            FmtTool::Goimports => matches!(
                name,
                "cpuprofile" | "local" | "memprofile" | "memrate" | "srcdir" | "trace"
            ),
        }
    }

    fn flags(self, args: &[String]) -> GoFlags<'_> {
        go_flags(args, &|_kind, name| {
            self.takes_value(name).then(ValueSpec::value)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Diff,
    Passthrough,
}

fn classify(tool: FmtTool, args: &[String]) -> Mode {
    let flags = tool.flags(args);
    let t = &flags.tokens;
    if wants_help(t) {
        Mode::Passthrough
    } else if bool_flag(t, "d") {
        Mode::Diff
    } else if bool_flag(t, "l") {
        Mode::List
    } else {
        Mode::Passthrough
    }
}

/// Without file arguments the tool formats stdin (`gofmt -d < x.go`).
fn reads_stdin(tool: FmtTool, args: &[String]) -> bool {
    tool.flags(args).rest == args.len()
}

pub fn run(tool: FmtTool, args: &[String], verbose: u8) -> Result<i32> {
    let args = args_utils::restore_double_dash(args);
    let name = tool.name();
    let mode = classify(tool, &args);
    if mode == Mode::Passthrough {
        let os_args: Vec<OsString> = args.iter().map(OsString::from).collect();
        return runner::run_passthrough(name, &os_args, verbose);
    }
    let mut cmd = resolved_command(name);
    cmd.args(&args);
    if verbose > 0 {
        eprintln!("Running: {name} {}", args.join(" "));
    }
    let mut opts = runner::RunOptions::stdout_only().tee(name);
    // The runner closes stdin for filtered commands; a stdin run needs it passed on.
    if reads_stdin(tool, &args) {
        opts = opts.inherit_stdin();
    }
    runner::run_filtered_with_exit(
        cmd,
        name,
        &args.join(" "),
        move |output, exit_code| match mode {
            Mode::List => filter_list(output, exit_code, |content, offset| {
                tee::force_tee_tail_hint(content, name, offset)
            }),
            _ => match filter_diff(tool, output) {
                Some(summary) => append_hint(output, summary, exit_code, || {
                    tee::force_tee_hint(output, name)
                }),
                None => {
                    if !output.trim().is_empty() {
                        eprintln!(
                            "rtk: filter warning: {name} -d output is not a diff, showing it unchanged"
                        );
                    }
                    output.to_string()
                }
            },
        },
        opts,
    )
}

/// The first `MAX_FILES` paths exactly as the tool printed them, then the recall hint. The list
/// is often consumed (`for f in $(gofmt -l .)`), so rtk adds no header. Only capped when the
/// store answers, and only at exit 0: a failure's list stays whole beside the runner's tee.
fn filter_list(
    output: &str,
    exit_code: i32,
    store: impl FnOnce(&str, usize) -> Option<String>,
) -> String {
    if exit_code != 0 || output.lines().count() <= MAX_FILES {
        return output.to_string();
    }
    match store(output, MAX_FILES + 1) {
        Some(hint) => {
            let shown: Vec<&str> = output.lines().take(MAX_FILES).collect();
            format!("{}\n{hint}", shown.join("\n"))
        }
        None => output.to_string(),
    }
}

fn filter_diff(tool: FmtTool, output: &str) -> Option<String> {
    let mut files: Vec<(String, usize, usize)> = Vec::new();
    for line in output.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            files.push((path.split('\t').next().unwrap_or(path).to_string(), 0, 0));
            continue;
        }
        if line.starts_with("--- ") {
            continue;
        }
        let Some(current) = files.last_mut() else {
            continue;
        };
        if line.starts_with('+') {
            current.1 += 1;
        } else if line.starts_with('-') {
            current.2 += 1;
        }
    }
    if files.is_empty() {
        return None;
    }
    let noun = if files.len() == 1 { "file" } else { "files" };
    let mut out = vec![format!(
        "{} -d: {} {noun} would change",
        tool.name(),
        files.len()
    )];
    out.extend(
        files
            .iter()
            .take(MAX_FILES)
            .map(|(path, added, removed)| format!("  {path} (+{added} -{removed})")),
    );
    if files.len() > MAX_FILES {
        out.push(format!("  … +{} more", files.len() - MAX_FILES));
    }
    Some(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmds::go::go_run::test_support::{assert_savings, s};
    use std::cell::Cell;

    const LIST: &str = include_str!("../../../tests/fixtures/go_gofmt_l_raw.txt");
    const DIFF: &str = include_str!("../../../tests/fixtures/go_gofmt_d_raw.txt");
    const IMPORTS_LIST: &str = include_str!("../../../tests/fixtures/go_goimports_l_raw.txt");

    #[test]
    fn modes() {
        assert_eq!(classify(FmtTool::Gofmt, &s(&["-l", "."])), Mode::List);
        assert_eq!(classify(FmtTool::Gofmt, &s(&["-l", "-w", "."])), Mode::List);
        assert_eq!(classify(FmtTool::Gofmt, &s(&["-l", "-d", "."])), Mode::Diff);
        assert_eq!(
            classify(FmtTool::Gofmt, &s(&["-d=false", "-l", "."])),
            Mode::List
        );
        assert_eq!(
            classify(FmtTool::Gofmt, &s(&["-w", "."])),
            Mode::Passthrough
        );
        assert_eq!(
            classify(FmtTool::Gofmt, &s(&["main.go"])),
            Mode::Passthrough
        );
        assert_eq!(
            classify(FmtTool::Gofmt, &s(&["-r", "a -> b", "-l", "."])),
            Mode::List
        );
        assert_eq!(classify(FmtTool::Gofmt, &s(&["-h"])), Mode::Passthrough);
        // Go stops at the first file: a later `-l` is a file name.
        assert_eq!(
            classify(FmtTool::Gofmt, &s(&["main.go", "-l"])),
            Mode::Passthrough
        );
        assert_eq!(
            classify(
                FmtTool::Goimports,
                &s(&["-local", "example.com", "-l", "."])
            ),
            Mode::List
        );
    }

    #[test]
    fn stdin_is_read_only_without_file_arguments() {
        assert!(reads_stdin(FmtTool::Gofmt, &s(&["-l"])));
        assert!(reads_stdin(FmtTool::Gofmt, &s(&["-d", "-r", "a -> b"])));
        assert!(!reads_stdin(FmtTool::Gofmt, &s(&["-l", "."])));
        assert!(!reads_stdin(
            FmtTool::Goimports,
            &s(&["-local", "x", "-d", "main.go"])
        ));
    }

    #[test]
    fn long_lists_keep_gofmts_own_lines_and_hand_the_rest_to_recall() {
        let seen = Cell::new(0);
        let out = filter_list(LIST, 0, |content, offset| {
            assert_eq!(content, LIST);
            seen.set(offset);
            Some("[+5 hidden: rtk recall abc]".into())
        });
        assert_eq!(seen.get(), MAX_FILES + 1);
        let expected: Vec<&str> = LIST.lines().take(MAX_FILES).collect();
        assert_eq!(
            out,
            format!("{}\n[+5 hidden: rtk recall abc]", expected.join("\n"))
        );
    }

    #[test]
    fn lists_are_whole_without_a_store_or_within_the_cap() {
        assert_eq!(filter_list(LIST, 0, |_, _| None), LIST);
        let short: String = LIST.lines().take(3).map(|l| format!("{l}\n")).collect();
        assert_eq!(
            filter_list(&short, 0, |_, _| panic!("no store for a short list")),
            short
        );
        assert_eq!(
            filter_list("", 0, |_, _| panic!("no store for an empty list")),
            ""
        );
        assert_eq!(filter_list(IMPORTS_LIST, 0, |_, _| None), IMPORTS_LIST);
    }

    #[test]
    fn list_is_not_capped_on_failure() {
        assert_eq!(
            filter_list(LIST, 2, |_, _| panic!("failures keep the runner's tee")),
            LIST
        );
    }

    #[test]
    fn diffs_become_one_line_per_file() {
        assert_eq!(
            filter_diff(FmtTool::Gofmt, DIFF).expect("diff"),
            "gofmt -d: 3 files would change\n  f1.go (+4 -2)\n  f2.go (+4 -2)\n  f3.go (+4 -2)"
        );
        assert!(
            filter_diff(FmtTool::Goimports, DIFF)
                .expect("diff")
                .starts_with("goimports -d: 3 files")
        );
    }

    #[test]
    fn unicode_paths_survive() {
        let list: String = (0..25).map(|i| format!("pkg/файл_{i}.go\n")).collect();
        let out = filter_list(&list, 0, |_, _| Some("[+5 hidden: rtk recall x]".into()));
        assert!(out.starts_with("pkg/файл_0.go\npkg/файл_1.go\n"), "{out}");
        let diff = DIFF.replace("f1.go", "日本.go");
        assert!(
            filter_diff(FmtTool::Gofmt, &diff)
                .expect("diff")
                .contains("  日本.go (+4 -2)")
        );
    }

    #[test]
    fn non_diff_output_is_not_summarised() {
        assert_eq!(
            filter_diff(FmtTool::Gofmt, "bad.go:2:6: expected 'IDENT', found '{'\n"),
            None
        );
    }

    #[test]
    fn savings_on_diff() {
        assert_savings(
            "gofmt -d",
            DIFF,
            &filter_diff(FmtTool::Gofmt, DIFF).expect("diff"),
        );
    }
}
