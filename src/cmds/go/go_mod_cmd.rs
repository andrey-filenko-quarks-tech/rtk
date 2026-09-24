//! Filters `go mod` output: `graph` summarised to direct requirements and version
//! conflicts, `tidy` reported as a `go.mod` diff. Also hosts the Go flag and `go.mod` helpers
//! shared by the other Go subcommand modules.

use crate::core::arg_tokenizer::{self, Dialect, Token, TokenKind, ValueSpec};
use crate::core::guard::never_worse;
use crate::core::runner;
use crate::core::stream::exec_capture;
use crate::core::tee;
use crate::core::tracking;
use crate::core::truncate::CAP_LIST;
use crate::core::utils::resolved_command;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const MAX_GRAPH_ITEMS: usize = CAP_LIST;
const GRAPH_TEE_LABEL: &str = "go-mod-graph";
const MAX_TIDY_CHANGES: usize = CAP_LIST;
const TIDY_TEE_LABEL: &str = "go-mod-tidy";

/// Tokens Go's `flag` package would parse, and the index of the first argument it would not.
pub(crate) struct GoFlags<'a> {
    pub tokens: Vec<Token<'a>>,
    pub rest: usize,
}

/// Go's `flag` package takes atomic single-dash names (`-modfile`), and stops at `--` or at the
/// first non-flag argument. `Msbuild` gives atomic names; a `/abs/path` argument, which it reads
/// as a `/flag`, is a positional to Go.
pub(crate) fn go_flags<'a>(
    args: &'a [String],
    takes_value: &dyn Fn(TokenKind, &str) -> Option<ValueSpec>,
) -> GoFlags<'a> {
    let tokens = arg_tokenizer::tokenize_grammar(args, takes_value, Dialect::Msbuild);
    let mut own = Vec::new();
    for token in tokens {
        if token.kind == TokenKind::DashDash {
            return GoFlags {
                tokens: own,
                rest: token.source_index + 1,
            };
        }
        if token.slash || token.is_free_positional() {
            return GoFlags {
                tokens: own,
                rest: token.source_index,
            };
        }
        own.push(token);
    }
    GoFlags {
        tokens: own,
        rest: args.len(),
    }
}

// Exact comparison: `Msbuild`'s own matching ignores case, and Go's `-C` is not `-c`.
pub(crate) fn has_flag(tokens: &[Token<'_>], name: &str) -> bool {
    tokens
        .iter()
        .any(|t| t.kind == TokenKind::Long && t.text == name)
}

pub(crate) fn flag_value<'a>(tokens: &[Token<'a>], name: &str) -> Option<&'a str> {
    tokens
        .iter()
        .find(|t| t.kind == TokenKind::Long && t.text == name)
        .and_then(|t| t.value(tokens))
}

pub(crate) fn wants_help(tokens: &[Token<'_>]) -> bool {
    has_flag(tokens, "h") || has_flag(tokens, "help")
}

/// On exit 0 a summarised output carries its own recovery hint; on failure the runner's tee
/// stores the raw output. Skipped when `never_worse` would print the raw output anyway.
pub(crate) fn append_hint(
    raw: &str,
    filtered: String,
    exit_code: i32,
    store: impl FnOnce() -> Option<String>,
) -> String {
    if exit_code != 0 || filtered == raw || never_worse(raw, &filtered) == raw {
        return filtered;
    }
    match store() {
        Some(hint) => format!("{filtered}\n{hint}"),
        None => filtered,
    }
}

pub(crate) fn run_go_passthrough(sub: &str, args: &[String], verbose: u8) -> Result<i32> {
    let os_args: Vec<OsString> = std::iter::once(OsString::from(sub))
        .chain(args.iter().map(OsString::from))
        .collect();
    runner::run_passthrough("go", &os_args, verbose)
}

fn graph_takes_value(kind: TokenKind, name: &str) -> Option<ValueSpec> {
    (kind == TokenKind::Long && matches!(name, "C" | "modfile" | "go")).then(ValueSpec::value)
}

fn tidy_takes_value(kind: TokenKind, name: &str) -> Option<ValueSpec> {
    (kind == TokenKind::Long && matches!(name, "C" | "modfile" | "go" | "compat"))
        .then(ValueSpec::value)
}

#[derive(Debug, PartialEq, Eq)]
enum ModInvocation {
    Tidy {
        chdir: Option<String>,
        modfile: Option<String>,
    },
    Graph,
    Passthrough,
}

/// Flags only, no stray positional (the subcommands filtered here take none: a stray one is
/// Go's usage error to print), and no help request.
fn filterable(flags: &GoFlags<'_>, args: &[String]) -> bool {
    flags.rest == args.len() && !wants_help(&flags.tokens)
}

// `go mod` takes no flags before its subcommand, so the first argument is the subcommand.
fn classify(args: &[String]) -> ModInvocation {
    let Some((sub, rest)) = args.split_first() else {
        return ModInvocation::Passthrough;
    };
    match sub.as_str() {
        "graph" if filterable(&go_flags(rest, &graph_takes_value), rest) => ModInvocation::Graph,
        "tidy" => {
            let flags = go_flags(rest, &tidy_takes_value);
            // `-diff` prints what tidy would change and changes nothing: the user's own view.
            if !filterable(&flags, rest) || has_flag(&flags.tokens, "diff") {
                ModInvocation::Passthrough
            } else {
                ModInvocation::Tidy {
                    chdir: flag_value(&flags.tokens, "C").map(String::from),
                    modfile: flag_value(&flags.tokens, "modfile").map(String::from),
                }
            }
        }
        _ => ModInvocation::Passthrough,
    }
}

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let args = crate::core::args_utils::restore_double_dash(args);
    match classify(&args) {
        ModInvocation::Tidy { chdir, modfile } => {
            run_tidy(&args, chdir.as_deref(), modfile.as_deref(), verbose)
        }
        ModInvocation::Graph => run_graph(&args, verbose),
        ModInvocation::Passthrough => run_go_passthrough("mod", &args, verbose),
    }
}

fn run_graph(args: &[String], verbose: u8) -> Result<i32> {
    let mut cmd = resolved_command("go");
    cmd.arg("mod").args(args);
    if verbose > 0 {
        eprintln!("Running: go mod {}", args.join(" "));
    }
    runner::run_filtered_with_exit(
        cmd,
        "go mod",
        &args.join(" "),
        move |stdout, exit_code| {
            if verbose > 1 {
                eprintln!("{stdout}");
            }
            let filtered = filter_go_mod_graph(stdout);
            append_hint(stdout, filtered, exit_code, || {
                tee::force_tee_hint(stdout, GRAPH_TEE_LABEL)
            })
        },
        runner::RunOptions::stdout_only().tee(GRAPH_TEE_LABEL),
    )
}

/// Modules reachable from `start`, excluding `start` itself.
fn transitive_count(adjacency: &HashMap<&str, Vec<&str>>, start: &str) -> usize {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = adjacency
        .get(start)
        .into_iter()
        .flatten()
        .copied()
        .collect();
    while let Some(node) = queue.pop_front() {
        if node != start && seen.insert(node) {
            queue.extend(adjacency.get(node).into_iter().flatten().copied());
        }
    }
    seen.len()
}

/// Module paths seen at more than one version, versions in order of first appearance (string
/// order would put `v0.10` before `v0.9`).
fn version_conflicts<'a>(edges: &[(&'a str, &'a str)]) -> Vec<(&'a str, Vec<&'a str>)> {
    let mut order: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for node in edges.iter().flat_map(|(from, to)| [*from, *to]) {
        let Some((path, version)) = node.split_once('@') else {
            continue;
        };
        let slot = *index.entry(path).or_insert_with(|| {
            order.push((path, Vec::new()));
            order.len() - 1
        });
        if !order[slot].1.contains(&version) {
            order[slot].1.push(version);
        }
    }
    order
        .into_iter()
        .filter(|(_, versions)| versions.len() > 1)
        .collect()
}

fn push_capped(out: &mut String, lines: &[String]) {
    for line in lines.iter().take(MAX_GRAPH_ITEMS) {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    if lines.len() > MAX_GRAPH_ITEMS {
        out.push_str(&format!("  … +{} more\n", lines.len() - MAX_GRAPH_ITEMS));
    }
}

/// Summarise `go mod graph`: module and edge counts, the main module's direct requirements with
/// their transitive fan-out, and modules required at more than one version.
fn filter_go_mod_graph(stdout: &str) -> String {
    // An edge is exactly `from to@version`; anything else means output this filter does not
    // understand, which is returned as is.
    let Some(edges) = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            l.split_once(' ')
                .filter(|(_, to)| to.contains('@') && !to.contains(' '))
        })
        .collect::<Option<Vec<(&str, &str)>>>()
    else {
        return stdout.to_string();
    };
    // `go@1.25.0` / `toolchain@go1.25.1` record Go version requirements, not modules.
    let edges: Vec<(&str, &str)> = edges
        .into_iter()
        .filter(|(_, to)| !to.starts_with("go@") && !to.starts_with("toolchain@"))
        .collect();
    let Some(main) = edges
        .iter()
        .map(|(from, _)| *from)
        .find(|f| !f.contains('@'))
    else {
        return stdout.to_string();
    };
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut nodes: HashSet<&str> = HashSet::new();
    for (from, to) in &edges {
        adjacency.entry(from).or_default().push(to);
        nodes.insert(from);
        nodes.insert(to);
    }
    let mut direct: Vec<(&str, usize)> = adjacency
        .get(main)
        .into_iter()
        .flatten()
        .map(|d| (*d, transitive_count(&adjacency, d)))
        .collect();
    direct.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let conflicts = version_conflicts(&edges);

    let mut out = format!(
        "go mod graph: {} modules, {} edges (main {main})\ndirect ({}):\n",
        nodes.len(),
        edges.len(),
        direct.len()
    );
    let direct_lines: Vec<String> = direct
        .iter()
        .map(|(d, n)| format!("{d} (+{n} transitive)"))
        .collect();
    push_capped(&mut out, &direct_lines);
    if !conflicts.is_empty() {
        out.push_str(&format!("multiple versions ({}):\n", conflicts.len()));
        let conflict_lines: Vec<String> = conflicts
            .iter()
            .map(|(p, v)| format!("{p}: {}", v.join(", ")))
            .collect();
        push_capped(&mut out, &conflict_lines);
    }
    out.trim_end().to_string()
}

pub(crate) struct Require {
    pub path: String,
    pub version: String,
    pub indirect: bool,
}

fn parse_require_line(line: &str) -> Option<Require> {
    let (spec, comment) = match line.split_once("//") {
        Some((spec, comment)) => (spec, Some(comment.trim())),
        None => (line, None),
    };
    let mut parts = spec.split_whitespace();
    let path = parts.next()?;
    let version = parts.next()?;
    Some(Require {
        path: path.to_string(),
        version: version.to_string(),
        indirect: comment.is_some_and(|c| c == "indirect" || c.starts_with("indirect;")),
    })
}

/// `require` directives, block and single-line form; everything else is ignored.
pub(crate) fn parse_requires(go_mod: &str) -> Vec<Require> {
    let mut requires = Vec::new();
    let mut in_block = false;
    for line in go_mod.lines().map(str::trim) {
        if in_block {
            if line.starts_with(')') {
                in_block = false;
            } else {
                requires.extend(parse_require_line(line));
            }
            continue;
        }
        let Some(rest) = line.strip_prefix("require") else {
            continue;
        };
        if rest.starts_with([' ', '\t', '(']) && rest.trim_start().starts_with('(') {
            in_block = true;
        } else if rest.starts_with([' ', '\t']) {
            requires.extend(parse_require_line(rest));
        }
    }
    requires
}

/// Nearest `go.mod` walking up from `start`, as `go` itself resolves the main module.
pub(crate) fn find_go_mod(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join("go.mod"))
        .find(|p| p.is_file())
}

/// The `go.mod` a command run with `-C chdir` / `-modfile modfile` works on.
pub(crate) fn resolve_go_mod(chdir: Option<&str>, modfile: Option<&str>) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let base = chdir.map_or_else(|| cwd.clone(), |dir| cwd.join(dir));
    match modfile {
        Some(file) => Some(base.join(file)),
        None => find_go_mod(&base),
    }
}

pub(crate) fn read_requires(path: &Path) -> Option<Vec<Require>> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| parse_requires(&text))
}

struct TidyReport {
    changes: Vec<String>,
    added: usize,
    removed: usize,
    changed: usize,
    downloads: usize,
    kept: Vec<String>,
}

fn is_tidy_chatter(line: &str) -> bool {
    line.starts_with("go: downloading ")
        || (line.starts_with("go: finding ") && !line.starts_with("go: finding module for package"))
}

fn tidy_report(before: Option<&[Require]>, after: Option<&[Require]>, output: &str) -> TidyReport {
    let mut report = TidyReport {
        changes: Vec::new(),
        added: 0,
        removed: 0,
        changed: 0,
        downloads: 0,
        kept: Vec::new(),
    };
    for line in output.lines().filter(|l| !l.trim().is_empty()) {
        if is_tidy_chatter(line) {
            report.downloads += usize::from(line.starts_with("go: downloading "));
        } else {
            report.kept.push(line.to_string());
        }
    }
    let (Some(before), Some(after)) = (before, after) else {
        return report;
    };
    let old: HashMap<&str, &Require> = before.iter().map(|r| (r.path.as_str(), r)).collect();
    let new: HashMap<&str, &Require> = after.iter().map(|r| (r.path.as_str(), r)).collect();
    for r in after.iter().filter(|r| !old.contains_key(r.path.as_str())) {
        report.changes.push(format!("+ {} {}", r.path, r.version));
        report.added += 1;
    }
    for r in before.iter().filter(|r| !new.contains_key(r.path.as_str())) {
        report.changes.push(format!("- {} {}", r.path, r.version));
        report.removed += 1;
    }
    for r in after {
        let Some(prev) = old.get(r.path.as_str()) else {
            continue;
        };
        let flip = match (prev.indirect, r.indirect) {
            (false, true) => " (now indirect)",
            (true, false) => " (now direct)",
            _ => "",
        };
        if prev.version != r.version {
            report.changes.push(format!(
                "~ {} {} → {}{flip}",
                r.path, prev.version, r.version
            ));
        } else if !flip.is_empty() {
            report
                .changes
                .push(format!("~ {} {}{flip}", r.path, r.version));
        } else {
            continue;
        }
        report.changed += 1;
    }
    report
}

fn tidy_header(report: &TidyReport) -> Option<String> {
    let downloads = if report.downloads > 0 {
        format!(" ({} modules downloaded)", report.downloads)
    } else {
        String::new()
    };
    if !report.changes.is_empty() {
        Some(format!(
            "go mod tidy: +{} added, -{} removed, ~{} changed{downloads}",
            report.added, report.removed, report.changed
        ))
    } else if report.downloads > 0 {
        Some(format!("go mod tidy: no changes{downloads}"))
    } else {
        None
    }
}

fn render_tidy(report: &TidyReport, max: usize) -> String {
    let mut lines: Vec<String> = tidy_header(report).into_iter().collect();
    lines.extend(report.changes.iter().take(max).map(|c| format!("  {c}")));
    if report.changes.len() > max {
        lines.push(format!("  … +{} more", report.changes.len() - max));
    }
    lines.extend(report.kept.iter().cloned());
    lines.join("\n")
}

/// The diff is information the raw output never contains (tidy prints nothing about what it
/// changed), so it is exempt from `never_worse` — see `core/guard.rs`. Everything else is
/// guarded.
fn emit_tidy(report: &TidyReport, raw: &str) -> String {
    let rendered = render_tidy(report, MAX_TIDY_CHANGES);
    if report.changes.is_empty() {
        never_worse(raw, &rendered).to_string()
    } else {
        rendered
    }
}

/// On exit 0 with more changes than shown, the full formatted list is stored so recall returns
/// exactly the hidden tail (header line + shown lines, then the first hidden one).
fn tidy_tail_hint(
    report: &TidyReport,
    store: impl FnOnce(&str, usize) -> Option<String>,
) -> Option<String> {
    if report.changes.len() <= MAX_TIDY_CHANGES {
        return None;
    }
    let mut content: Vec<String> = tidy_header(report).into_iter().collect();
    content.extend(report.changes.iter().map(|c| format!("  {c}")));
    store(&content.join("\n"), 1 + MAX_TIDY_CHANGES + 1)
}

fn run_tidy(
    args: &[String],
    chdir: Option<&str>,
    modfile: Option<&str>,
    verbose: u8,
) -> Result<i32> {
    let timer = tracking::TimedExecution::start();
    let go_mod = resolve_go_mod(chdir, modfile);
    let before = go_mod.as_deref().and_then(read_requires);
    let mut cmd = resolved_command("go");
    cmd.arg("mod").args(args);
    if verbose > 0 {
        eprintln!("Running: go mod {}", args.join(" "));
    }
    let captured = exec_capture(&mut cmd).context("Failed to run go mod tidy")?;
    let raw = format!("{}{}", captured.stdout, captured.stderr);
    if verbose > 1 {
        eprintln!("{raw}");
    }
    let after = go_mod.as_deref().and_then(read_requires);
    let report = tidy_report(before.as_deref(), after.as_deref(), &raw);
    let mut shown = emit_tidy(&report, &raw);
    let hint = if captured.exit_code != 0 {
        tee::tee_and_hint(&raw, TIDY_TEE_LABEL, captured.exit_code)
    } else {
        tidy_tail_hint(&report, |content, offset| {
            tee::force_tee_tail_hint(content, TIDY_TEE_LABEL, offset)
        })
    };
    if let Some(hint) = hint {
        shown = if shown.is_empty() {
            hint
        } else {
            format!("{shown}\n{hint}")
        };
    }
    if !shown.is_empty() {
        println!("{shown}");
    }
    let label = format!("go mod {}", args.join(" "));
    timer.track(&label, &format!("rtk {label}"), &raw, &shown);
    Ok(captured.exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn s(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    pub(crate) fn count_tokens(text: &str) -> usize {
        text.split_whitespace().count()
    }

    pub(crate) fn assert_savings(name: &str, input: &str, output: &str) {
        let savings = 100.0 - (count_tokens(output) as f64 / count_tokens(input) as f64 * 100.0);
        eprintln!("{name}: {savings:.1}% bash output reduction");
        assert!(
            savings >= 60.0,
            "{name}: expected >=60% reduction, got {savings:.1}%"
        );
    }

    fn graph_values(kind: TokenKind, name: &str) -> Option<ValueSpec> {
        graph_takes_value(kind, name)
    }

    #[test]
    fn go_flags_keeps_single_dash_names_whole() {
        let args = s(&["-modfile=tools/go.mod", "-C", "sub", "-x"]);
        let flags = go_flags(&args, &graph_values);
        assert!(has_flag(&flags.tokens, "modfile"));
        assert!(has_flag(&flags.tokens, "C"));
        assert!(has_flag(&flags.tokens, "x"));
        assert!(!has_flag(&flags.tokens, "m"));
        // `-C`'s value is consumed, not a free positional.
        assert_eq!(flags.rest, 4);
    }

    #[test]
    fn go_flags_stops_at_the_first_positional_and_dashdash() {
        let args = s(&["-x", "./...", "-json"]);
        let flags = go_flags(&args, &graph_values);
        assert_eq!(flags.rest, 1);
        assert!(!has_flag(&flags.tokens, "json"));
        let args = s(&["-x", "--", "-json"]);
        assert_eq!(go_flags(&args, &graph_values).rest, 2);
    }

    #[test]
    fn go_flags_treats_an_absolute_path_as_a_positional() {
        let args = s(&["/abs/dir/...", "-x"]);
        let flags = go_flags(&args, &graph_values);
        assert_eq!(flags.rest, 0);
        assert!(flags.tokens.is_empty());
    }

    #[test]
    fn flag_names_are_case_sensitive() {
        let args = s(&["-c"]);
        assert!(!has_flag(&go_flags(&args, &graph_values).tokens, "C"));
    }

    #[test]
    fn classifies_mod_subcommands() {
        assert_eq!(classify(&s(&["graph"])), ModInvocation::Graph);
        assert_eq!(classify(&s(&["graph", "-go=1.22"])), ModInvocation::Graph);
        // `graph` takes no arguments: a stray one is Go's error to print.
        for args in [
            &["why", "x"][..],
            &["download"],
            &[],
            &["graph", "-help"],
            &["--help"],
            &["graph", "extra"],
        ] {
            assert_eq!(classify(&s(args)), ModInvocation::Passthrough, "{args:?}");
        }
    }

    #[test]
    fn graph_summarises_direct_requirements_and_version_conflicts() {
        let raw = "\
example.com/main a.io/x@v1.0.0
example.com/main b.io/y@v2.0.0
a.io/x@v1.0.0 c.io/z@v0.9.0
a.io/x@v1.0.0 d.io/w@v1.0.0
c.io/z@v0.9.0 d.io/w@v1.1.0
b.io/y@v2.0.0 c.io/z@v0.10.0
";
        assert_eq!(
            filter_go_mod_graph(raw),
            "\
go mod graph: 7 modules, 6 edges (main example.com/main)
direct (2):
  a.io/x@v1.0.0 (+3 transitive)
  b.io/y@v2.0.0 (+1 transitive)
multiple versions (2):
  c.io/z: v0.9.0, v0.10.0
  d.io/w: v1.0.0, v1.1.0"
        );
    }

    #[test]
    fn graph_caps_sections() {
        let raw: String = (0..25)
            .map(|i| format!("example.com/main m{i:02}.io/x@v1.0.0\n"))
            .collect();
        let out = filter_go_mod_graph(&raw);
        assert!(out.contains("direct (25):"), "{out}");
        assert!(out.contains("  … +5 more"), "{out}");
        assert!(!out.contains("multiple versions"), "{out}");
    }

    #[test]
    fn graph_ignores_go_and_toolchain_version_nodes() {
        let raw = "\
example.com/main go@1.25.0
example.com/main toolchain@go1.25.1
example.com/main a.io/x@v1.0.0
a.io/x@v1.0.0 go@1.21
";
        assert_eq!(
            filter_go_mod_graph(raw),
            "go mod graph: 2 modules, 1 edges (main example.com/main)\ndirect (1):\n  a.io/x@v1.0.0 (+0 transitive)"
        );
    }

    #[test]
    fn graph_without_edges_is_unchanged() {
        assert_eq!(filter_go_mod_graph(""), "");
        assert_eq!(filter_go_mod_graph("go: error\n"), "go: error\n");
    }

    #[test]
    fn append_hint_only_on_a_smaller_summary_at_exit_zero() {
        let raw = "a b c d e f g h i j k l m n o p q r s t u v w x y z\n".repeat(5);
        let hinted = append_hint(&raw, "sum".into(), 0, || {
            Some("[full output: rtk recall x]".into())
        });
        assert_eq!(hinted, "sum\n[full output: rtk recall x]");
        let never = || -> Option<String> { panic!("nothing should be stored") };
        assert_eq!(append_hint(&raw, "sum".into(), 1, never), "sum");
        assert_eq!(
            append_hint("a", "a much longer summary".into(), 0, never),
            "a much longer summary"
        );
        assert_eq!(append_hint(&raw, raw.clone(), 0, never), raw);
    }

    #[test]
    fn graph_fixture() {
        let input = include_str!("../../../tests/fixtures/go_mod_graph_raw.txt");
        let out = filter_go_mod_graph(input);
        assert_eq!(
            out,
            "\
go mod graph: 204 modules, 405 edges (main google.golang.org/grpc)
direct (42):
  google.golang.org/api@v0.293.0 (+85 transitive)
  cloud.google.com/go/auth@v0.23.1 (+78 transitive)
  github.com/envoyproxy/go-control-plane/envoy@v1.39.1-0.20260819172001-e6e3fd93e4be (+67 transitive)
  github.com/envoyproxy/go-control-plane@v0.14.0 (+39 transitive)
  github.com/google/s2a-go@v0.1.9 (+36 transitive)
  github.com/googleapis/gax-go/v2@v2.23.0 (+32 transitive)
  go.opentelemetry.io/contrib/detectors/gcp@v1.45.0 (+30 transitive)
  go.opentelemetry.io/contrib/instrumentation/net/http/otelhttp@v0.70.0 (+26 transitive)
  go.opentelemetry.io/otel/sdk/metric@v1.45.0 (+24 transitive)
  go.opentelemetry.io/otel/sdk@v1.45.0 (+24 transitive)
  github.com/envoyproxy/go-control-plane/ratelimit@v0.1.0 (+21 transitive)
  go.opentelemetry.io/otel/metric@v1.45.0 (+18 transitive)
  go.opentelemetry.io/otel/trace@v1.45.0 (+18 transitive)
  go.opentelemetry.io/otel@v1.45.0 (+18 transitive)
  gonum.org/v1/gonum@v0.17.0 (+18 transitive)
  github.com/spiffe/go-spiffe/v2@v2.8.1 (+15 transitive)
  github.com/GoogleCloudPlatform/opentelemetry-operations-go/detectors/gcp@v1.35.0 (+11 transitive)
  github.com/planetscale/vtprotobuf@v0.6.1-0.20240319094008-0393e58bdf10 (+11 transitive)
  github.com/cncf/xds/go@v0.0.0-20260202195803-dba9d589def2 (+9 transitive)
  github.com/envoyproxy/protoc-gen-validate@v1.3.3 (+9 transitive)
  … +22 more
multiple versions (40):
  cel.dev/expr: v0.25.3, v0.25.1, v0.24.0, v0.25.2
  cloud.google.com/go/auth: v0.23.1, v0.3.0, v0.23.0
  cloud.google.com/go/compute/metadata: v0.9.0, v0.3.0
  github.com/cncf/xds/go: v0.0.0-20260202195803-dba9d589def2, v0.0.0-20250501225837-2ac532fd4443, v0.0.0-20240723142845-024c85f92f20
  github.com/envoyproxy/go-control-plane/envoy: v1.39.1-0.20260819172001-e6e3fd93e4be, v1.36.0, v1.32.2
  github.com/envoyproxy/protoc-gen-validate: v1.3.3, v1.3.0, v1.2.1, v1.1.0
  github.com/felixge/httpsnoop: v1.1.0, v1.0.4
  github.com/go-logr/logr: v1.4.4, v1.4.3, v1.2.2, v1.4.1
  github.com/golang/protobuf: v1.5.4, v1.5.3, v1.5.0
  github.com/google/go-cmp: v0.7.0, v0.6.0, v0.5.5
  github.com/googleapis/enterprise-certificate-proxy: v0.3.21, v0.3.17, v0.3.2, v0.3.20
  github.com/googleapis/gax-go/v2: v2.23.0, v2.12.3
  go.opentelemetry.io/contrib/instrumentation/net/http/otelhttp: v0.70.0, v0.67.0, v0.49.0
  go.opentelemetry.io/otel: v1.45.0, v1.44.0, v1.24.0, v1.38.0
  go.opentelemetry.io/otel/metric: v1.45.0, v1.44.0, v1.24.0
  go.opentelemetry.io/otel/sdk: v1.45.0, v1.44.0
  go.opentelemetry.io/otel/sdk/metric: v1.45.0, v1.44.0
  go.opentelemetry.io/otel/trace: v1.45.0, v1.44.0, v1.24.0, v1.38.0
  golang.org/x/crypto: v0.55.0, v0.53.0, v0.31.0, v0.52.0, v0.54.0
  golang.org/x/net: v0.58.0, v0.56.0, v0.42.0, v0.57.0, v0.28.0, v0.49.0, v0.33.0, v0.14.0, v0.48.0, v0.55.0
  … +20 more"
        );
        assert_savings("go mod graph", input, &out);
    }

    #[test]
    fn flag_values_are_read() {
        let args = s(&["-modfile=tools/go.mod", "-C", "sub"]);
        let flags = go_flags(&args, &tidy_takes_value);
        assert_eq!(flag_value(&flags.tokens, "modfile"), Some("tools/go.mod"));
        assert_eq!(flag_value(&flags.tokens, "C"), Some("sub"));
        assert_eq!(flag_value(&flags.tokens, "go"), None);
    }

    #[test]
    fn classifies_tidy() {
        assert_eq!(
            classify(&s(&["tidy"])),
            ModInvocation::Tidy {
                chdir: None,
                modfile: None
            }
        );
        assert_eq!(
            classify(&s(&["tidy", "-modfile=tools/go.mod", "-e"])),
            ModInvocation::Tidy {
                chdir: None,
                modfile: Some("tools/go.mod".into())
            }
        );
        assert_eq!(
            classify(&s(&["tidy", "-C", "sub", "-modfile", "x.mod"])),
            ModInvocation::Tidy {
                chdir: Some("sub".into()),
                modfile: Some("x.mod".into())
            }
        );
        assert_eq!(classify(&s(&["tidy", "-diff"])), ModInvocation::Passthrough);
        assert_eq!(classify(&s(&["tidy", "extra"])), ModInvocation::Passthrough);
    }

    #[test]
    fn parses_block_and_single_line_requires() {
        let go_mod = "\
module example.com/m

require a.io/x v1.0.0

require (
\tb.io/y v2.0.0 // indirect
\tc.io/z v0.1.0 // some note
)

replace a.io/x => ../x
exclude d.io/w v1.0.0
requirements.io/nope v1
";
        let got: Vec<(String, String, bool)> = parse_requires(go_mod)
            .into_iter()
            .map(|r| (r.path, r.version, r.indirect))
            .collect();
        assert_eq!(
            got,
            vec![
                ("a.io/x".into(), "v1.0.0".into(), false),
                ("b.io/y".into(), "v2.0.0".into(), true),
                ("c.io/z".into(), "v0.1.0".into(), false),
            ]
        );
    }

    #[test]
    fn finds_go_mod_walking_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("go.mod"), "module m\n").expect("write");
        let deep = dir.path().join("a/b");
        std::fs::create_dir_all(&deep).expect("mkdir");
        assert_eq!(find_go_mod(&deep), Some(dir.path().join("go.mod")));
        assert_eq!(find_go_mod(std::path::Path::new("/")), None);
    }

    fn req(path: &str, version: &str, indirect: bool) -> Require {
        Require {
            path: path.into(),
            version: version.into(),
            indirect,
        }
    }

    #[test]
    fn tidy_reports_changes_and_hides_chatter() {
        let before = [
            req("a.io/x", "v1.0.0", false),
            req("b.io/y", "v1.0.0", false),
            req("c.io/z", "v1.0.0", false),
        ];
        let after = [
            req("a.io/x", "v1.1.0", false),
            req("c.io/z", "v1.0.0", true),
            req("d.io/w", "v0.2.0", true),
        ];
        let output = "go: downloading a.io/x v1.1.0\ngo: finding module for package d.io/w\ngo: found d.io/w in d.io/w v0.2.0\n";
        let report = tidy_report(Some(&before[..]), Some(&after[..]), output);
        assert_eq!(
            render_tidy(&report, MAX_TIDY_CHANGES),
            "\
go mod tidy: +1 added, -1 removed, ~2 changed (1 modules downloaded)
  + d.io/w v0.2.0
  - b.io/y v1.0.0
  ~ a.io/x v1.0.0 → v1.1.0
  ~ c.io/z v1.0.0 (now indirect)
go: finding module for package d.io/w
go: found d.io/w in d.io/w v0.2.0"
        );
    }

    #[test]
    fn warm_tidy_without_changes_is_silent() {
        let mods = [req("a.io/x", "v1.0.0", false)];
        let report = tidy_report(Some(&mods[..]), Some(&mods[..]), "");
        assert_eq!(emit_tidy(&report, ""), "");
    }

    #[test]
    fn tidy_without_go_mod_only_counts_downloads() {
        let raw = "go: downloading a.io/x v1.0.0\ngo: downloading b.io/y v1.0.0\n";
        let report = tidy_report(None, None, raw);
        assert_eq!(
            emit_tidy(&report, raw),
            "go mod tidy: no changes (2 modules downloaded)"
        );
    }

    #[test]
    fn tidy_diff_may_exceed_the_empty_raw_output() {
        let before = [req("a.io/x", "v1.0.0", false)];
        let after = [req("a.io/x", "v1.1.0", false)];
        let report = tidy_report(Some(&before[..]), Some(&after[..]), "");
        assert_eq!(
            emit_tidy(&report, ""),
            "go mod tidy: +0 added, -0 removed, ~1 changed\n  ~ a.io/x v1.0.0 → v1.1.0"
        );
    }

    #[test]
    fn tidy_caps_changes_and_stores_the_full_list() {
        let after: Vec<Require> = (0..25)
            .map(|i| req(&format!("m{i:02}.io/x"), "v1.0.0", false))
            .collect();
        let report = tidy_report(Some(&[][..]), Some(after.as_slice()), "");
        let shown = render_tidy(&report, MAX_TIDY_CHANGES);
        assert!(shown.ends_with("  … +5 more"), "{shown}");
        let stored = std::cell::RefCell::new(None);
        let hint = tidy_tail_hint(&report, |content, offset| {
            stored.replace(Some((content.to_string(), offset)));
            Some("[+5 hidden: rtk recall x]".into())
        });
        assert_eq!(hint.as_deref(), Some("[+5 hidden: rtk recall x]"));
        let (content, offset) = stored.into_inner().expect("stored");
        assert_eq!(offset, 1 + MAX_TIDY_CHANGES + 1);
        assert_eq!(content.lines().count(), 26);
        let few = tidy_report(Some(&[][..]), Some(&after[..3]), "");
        assert!(tidy_tail_hint(&few, |_, _| panic!("no store")).is_none());
    }

    #[test]
    fn tidy_fixture_diff() {
        let before = parse_requires(include_str!(
            "../../../tests/fixtures/go_mod_tidy_before.mod"
        ));
        let after = parse_requires(include_str!(
            "../../../tests/fixtures/go_mod_tidy_after.mod"
        ));
        let report = tidy_report(Some(before.as_slice()), Some(after.as_slice()), "");
        assert_eq!(
            render_tidy(&report, MAX_TIDY_CHANGES),
            "\
go mod tidy: +1 added, -1 removed, ~1 changed
  + github.com/google/uuid v1.6.0
  - github.com/pkg/errors v0.9.1
  ~ golang.org/x/text v0.3.0 → v0.41.0"
        );
    }

    #[test]
    fn tidy_cold_fixture() {
        let input = include_str!("../../../tests/fixtures/go_mod_tidy_cold_raw.txt");
        let report = tidy_report(None, None, input);
        let out = emit_tidy(&report, input);
        assert!(out.starts_with("go mod tidy: no changes ("), "{out}");
        assert_savings("go mod tidy (cold)", input, &out);
    }
}
