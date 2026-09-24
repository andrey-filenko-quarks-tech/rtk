//! Groups govulncheck's called vulnerabilities by module, because the action is "upgrade this
//! module": one line per module with the highest fix version and one example trace.

use crate::cmds::go::go_args::{bool_flag, flag_value, go_flags, wants_help};
use crate::cmds::go::go_tool::ToolBin;
use crate::core::arg_tokenizer::{TokenKind, ValueSpec};
use crate::core::args_utils;
use crate::core::runner;
use crate::core::truncate::{self, CAP_ERRORS, CAP_WARNINGS};
use anyhow::Result;
use regex::Regex;
use std::sync::LazyLock;

const TOOL: &str = "govulncheck";
const MAX_MODULES: usize = CAP_ERRORS;
// IDs share one line; five identify the advisories, the full list is in the runner's tee.
const MAX_IDS: usize = truncate::reduced(CAP_WARNINGS, 5);

static ALSO_FOUND_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"also found (\d+) vulnerabilit(?:y|ies) in packages you import and (\d+) vulnerabilit(?:y|ies) in modules you require",
    )
    .unwrap()
});

#[derive(Debug, Default)]
struct Vuln {
    id: String,
    module: String,
    found: Option<String>,
    fixed: Option<String>,
    trace: Option<String>,
}

fn takes_value(_kind: TokenKind, name: &str) -> Option<ValueSpec> {
    matches!(
        name,
        "C" | "db" | "format" | "mode" | "scan" | "show" | "tags"
    )
    .then(ValueSpec::value)
}

fn passes_through(args: &[String]) -> bool {
    let flags = go_flags(args, &takes_value);
    let t = &flags.tokens;
    let show = flag_value(t, "show").unwrap_or("");
    wants_help(t)
        || bool_flag(t, "json")
        || bool_flag(t, "version")
        || flag_value(t, "format").is_some_and(|f| f != "text")
        || flag_value(t, "mode").is_some_and(|m| m != "source")
        || flag_value(t, "scan").is_some_and(|s| s != "symbol")
        || show
            .split(',')
            .any(|s| matches!(s.trim(), "verbose" | "traces" | "color"))
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
    cmd.args(args);
    let tool_name = bin.tool_name(TOOL);
    if verbose > 0 {
        eprintln!("Running: {tool_name} {}", args.join(" "));
    }
    runner::run_filtered(
        cmd,
        &tool_name,
        &args.join(" "),
        move |output| match filter_govulncheck(output) {
            Some(filtered) => {
                if verbose > 0 {
                    eprintln!(
                        "rtk govulncheck: {} lines in, {} out",
                        output.lines().count(),
                        filtered.lines().count()
                    );
                }
                filtered
            }
            None => {
                if !output.trim().is_empty() {
                    eprintln!(
                        "rtk: filter warning: govulncheck output not recognised, showing it unchanged"
                    );
                }
                output.to_string()
            }
        },
        runner::RunOptions::stdout_only().tee(TOOL),
    )
}

/// `None` when the output is not a symbol-level report rtk recognises.
fn filter_govulncheck(output: &str) -> Option<String> {
    let start = output.find("=== Symbol Results ===")?;
    let body = &output[start..];
    if body.contains("No vulnerabilities found.") {
        return Some(output.to_string());
    }
    let vulns = parse_vulns(body);
    if vulns.is_empty() {
        return None;
    }
    let summary_at = body.find("Your code is affected by").unwrap_or(body.len());
    // govulncheck wraps its summary at a fixed width, so the counts can land on any line.
    let paragraph = body[summary_at..]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let also = ALSO_FOUND_RE
        .captures(&paragraph)
        .map(|c| (c[1].to_string(), c[2].to_string()));
    Some(render(&vulns, also))
}

fn after_at(s: &str) -> &str {
    s.rsplit_once('@').map_or(s, |(_, v)| v)
}

fn parse_vulns(body: &str) -> Vec<Vuln> {
    let mut vulns: Vec<Vuln> = Vec::new();
    let mut trace_next = false;
    for line in body.lines() {
        let t = line.trim();
        if t.starts_with("Your code is affected by") {
            break;
        }
        if let Some(rest) = t.strip_prefix("Vulnerability #") {
            if let Some((_, id)) = rest.split_once(": ") {
                vulns.push(Vuln {
                    id: id.trim().to_string(),
                    ..Vuln::default()
                });
            }
            trace_next = false;
            continue;
        }
        let Some(v) = vulns.last_mut() else {
            continue;
        };
        if let Some(m) = t.strip_prefix("Module: ") {
            v.module = m.to_string();
        } else if t == "Standard library" {
            v.module = "stdlib".to_string();
        } else if let Some(f) = t.strip_prefix("Found in: ") {
            v.found = Some(after_at(f).to_string());
        } else if let Some(f) = t.strip_prefix("Fixed in: ") {
            v.fixed = (f != "N/A").then(|| after_at(f).to_string());
        } else if t == "Example traces found:" {
            trace_next = true;
        } else if trace_next {
            if v.trace.is_none() {
                v.trace = Some(t.split_once(": ").map_or(t, |(_, r)| r).to_string());
            }
            trace_next = false;
        }
    }
    vulns
}

fn version_key(version: &str) -> Vec<u64> {
    let v = version.trim_start_matches('v');
    let v = v.strip_prefix("go").unwrap_or(v);
    v.split(['-', '+'])
        .next()
        .unwrap_or("")
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

fn render(vulns: &[Vuln], also: Option<(String, String)>) -> String {
    let mut modules: Vec<(&str, Vec<&Vuln>)> = Vec::new();
    for v in vulns {
        match modules.iter_mut().find(|(m, _)| *m == v.module) {
            Some((_, members)) => members.push(v),
            None => modules.push((v.module.as_str(), vec![v])),
        }
    }
    let mut out = vec![format!(
        "govulncheck: {} called by your code, in {}",
        count(vulns.len(), "vulnerability", "vulnerabilities"),
        count(modules.len(), "module", "modules")
    )];
    for (module, members) in modules.iter().take(MAX_MODULES) {
        let found = members.iter().find_map(|v| v.found.as_deref());
        let best = members
            .iter()
            .filter_map(|v| v.fixed.as_deref())
            .max_by_key(|f| version_key(f));
        let unfixed = members.iter().filter(|v| v.fixed.is_none()).count();
        let fix = match (best, unfixed) {
            (Some(f), 0) => format!("≥ {f}"),
            (Some(f), n) => format!("≥ {f}, {n} with no fix yet"),
            (None, _) => "no fix yet".to_string(),
        };
        let ids: Vec<&str> = members
            .iter()
            .take(MAX_IDS)
            .map(|v| v.id.as_str())
            .collect();
        let rest = members.len().saturating_sub(MAX_IDS);
        let extra = if rest > 0 {
            format!(", +{rest}")
        } else {
            String::new()
        };
        let head = match found {
            Some(f) => format!("  {module} {f}"),
            None => format!("  {module}"),
        };
        out.push(format!(
            "{head} → {fix} ({}: {}{extra})",
            members.len(),
            ids.join(", ")
        ));
        if let Some(trace) = members.iter().find_map(|v| v.trace.as_deref()) {
            out.push(format!("    {trace}"));
        }
    }
    if modules.len() > MAX_MODULES {
        out.push(format!("  … +{} more modules", modules.len() - MAX_MODULES));
    }
    if let Some((imported, required)) = also {
        out.push(format!(
            "also: {imported} in imported packages, {required} in required modules, not called"
        ));
    }
    out.join("\n")
}

fn count(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmds::go::go_run::test_support::{assert_savings, s};

    const VULN: &str = include_str!("../../../tests/fixtures/go_govulncheck_vuln_raw.txt");
    const CLEAN: &str = include_str!("../../../tests/fixtures/go_govulncheck_clean_raw.txt");

    #[test]
    fn detailed_and_machine_forms_pass_through() {
        for args in [
            &["-format", "json", "./..."][..],
            &["-format=sarif", "./..."],
            &["-json", "./..."],
            &["-show", "verbose", "./..."],
            &["-show=traces,color", "./..."],
            &["-mode", "binary", "app"],
            &["-scan", "module"],
            &["-version"],
            &["-h"],
        ] {
            assert!(passes_through(&s(args)), "{args:?}");
        }
        for args in [
            &["./..."][..],
            &["-format", "text", "./..."],
            &["-scan=symbol", "./..."],
            &["-C", "sub", "-test", "./..."],
            &["-show", "version", "./..."],
        ] {
            assert!(!passes_through(&s(args)), "{args:?}");
        }
    }

    #[test]
    fn groups_vulnerabilities_by_module() {
        assert_eq!(
            filter_govulncheck(VULN).expect("parses"),
            "govulncheck: 9 vulnerabilities called by your code, in 1 module\n\
             \x20 golang.org/x/net v0.0.0-20220127200216-cd36cc0744dd → ≥ v0.55.0 (9: GO-2026-5030, GO-2026-5029, GO-2026-5028, GO-2026-5027, GO-2026-5025, +4)\n\
             \x20   main.go:12:20: vuln.main calls html.Parse\n\
             also: 2 in imported packages, 10 in required modules, not called"
        );
    }

    #[test]
    fn a_clean_run_is_left_alone() {
        assert_eq!(filter_govulncheck(CLEAN).as_deref(), Some(CLEAN));
    }

    #[test]
    fn unrecognised_output_is_reported_as_such() {
        assert_eq!(
            filter_govulncheck("govulncheck: loading packages: exit status 1\n"),
            None
        );
        assert_eq!(filter_govulncheck(""), None);
        assert_eq!(filter_govulncheck("=== Symbol Results ===\n\n"), None);
    }

    #[test]
    fn fix_versions_compare_numerically() {
        assert!(version_key("v0.55.0") > version_key("v0.9.0"));
        assert!(version_key("go1.21.10") > version_key("go1.21.5"));
        assert_eq!(
            version_key("v1.2.3-0.20220127200216-cd36cc0744dd"),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn stdlib_and_unfixed_vulnerabilities() {
        let input = "=== Symbol Results ===\n\n\
            Vulnerability #1: GO-2025-0001\n    Something in crypto/x509\n  More info: https://pkg.go.dev/vuln/GO-2025-0001\n\
            \x20 Standard library\n    Found in: crypto/x509@go1.21\n    Fixed in: crypto/x509@go1.21.5\n    Example traces found:\n      #1: main.go:5:2: app.main calls x509.ParseCertificate\n\n\
            Vulnerability #2: GO-2025-0002\n    Other in example.com/lib\n  More info: https://pkg.go.dev/vuln/GO-2025-0002\n\
            \x20 Module: example.com/lib\n    Found in: example.com/lib@v1.0.0\n    Fixed in: N/A\n    Example traces found:\n      #1: main.go:9:3: app.main calls lib.Do\n\n\
            Your code is affected by 2 vulnerabilities from the Go standard library and 1 module.\n";
        let out = filter_govulncheck(input).expect("parses");
        assert!(
            out.contains("  stdlib go1.21 → ≥ go1.21.5 (1: GO-2025-0001)"),
            "{out}"
        );
        assert!(
            out.contains("  example.com/lib v1.0.0 → no fix yet (1: GO-2025-0002)"),
            "{out}"
        );
        assert!(!out.contains("also:"), "{out}");
    }

    #[test]
    fn summary_counts_survive_any_wrapping() {
        let rewrapped = VULN.replace(
            "This scan also found 2 vulnerabilities in packages you import and 10\nvulnerabilities",
            "This scan also found 2\nvulnerabilities in packages you import and 10 vulnerabilities",
        );
        assert_ne!(rewrapped, VULN, "fixture wording changed");
        assert!(
            filter_govulncheck(&rewrapped)
                .expect("parses")
                .ends_with("also: 2 in imported packages, 10 in required modules, not called")
        );
    }

    #[test]
    fn savings_on_vulnerable_probe() {
        assert_savings(
            "govulncheck",
            VULN,
            &filter_govulncheck(VULN).expect("parses"),
        );
    }
}
