//! Filters `go mod` output: `graph` summarised to direct requirements and version
//! conflicts. Also hosts the Go flag helper shared by the other Go subcommand modules.

use crate::core::arg_tokenizer::{self, Dialect, Token, TokenKind, ValueSpec};
use crate::core::guard::never_worse;
use crate::core::runner;
use crate::core::tee;
use crate::core::truncate::CAP_LIST;
use crate::core::utils::resolved_command;
use anyhow::Result;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;

const MAX_GRAPH_ITEMS: usize = CAP_LIST;
const GRAPH_TEE_LABEL: &str = "go-mod-graph";

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

#[derive(Debug, PartialEq, Eq)]
enum ModInvocation {
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
        _ => ModInvocation::Passthrough,
    }
}

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let args = crate::core::args_utils::restore_double_dash(args);
    match classify(&args) {
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
}
