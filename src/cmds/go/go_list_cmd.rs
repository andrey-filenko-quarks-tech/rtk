//! Filters `go list`: package lists print the shared module path once, `-m all` shows direct
//! requirements, `-m -u all` shows only modules with updates. Machine-shaped forms pass through.

use crate::cmds::go::go_mod_cmd::{
    self, Require, append_hint, flag_value, go_flags, has_flag, wants_help,
};
use crate::core::arg_tokenizer::{TokenKind, ValueSpec};
use crate::core::runner;
use crate::core::tee;
use crate::core::truncate::{CAP_INVENTORY, CAP_LIST};
use crate::core::utils::resolved_command;
use anyhow::Result;
use std::collections::HashSet;

const MAX_PACKAGES: usize = CAP_INVENTORY;
const MAX_MODULES: usize = CAP_INVENTORY;
const MAX_UPDATES: usize = CAP_LIST;
const LIST_TEE_LABEL: &str = "go-list";

/// Flags whose output is meant for a program, not a reader: never reshaped.
const MACHINE_FLAGS: &[&str] = &[
    "f",
    "json",
    "e",
    "versions",
    "retracted",
    "reuse",
    "find",
    "deps",
    "test",
    "compiled",
    "export",
];

/// `go list` plus `go help build`'s value-taking flags (go 1.27.1).
fn list_takes_value(kind: TokenKind, name: &str) -> Option<ValueSpec> {
    if kind != TokenKind::Long {
        return None;
    }
    match name {
        // `-json[=fields]` and `-buildvcs[=bool]` take an optional attached value only.
        "json" | "buildvcs" => Some(ValueSpec::attached_only()),
        "C" | "f" | "p" | "covermode" | "coverpkg" | "asmflags" | "buildmode" | "compiler"
        | "gccgoflags" | "gcflags" | "installsuffix" | "ldflags" | "mod" | "modfile"
        | "overlay" | "pgo" | "pkgdir" | "reuse" | "tags" | "toolexec" => Some(ValueSpec::value()),
        _ => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ListInvocation {
    Packages,
    ModulesAll,
    ModulesUpdates,
    Passthrough,
}

fn classify(args: &[String]) -> ListInvocation {
    let flags = go_flags(args, &list_takes_value);
    if wants_help(&flags.tokens) || MACHINE_FLAGS.iter().any(|f| has_flag(&flags.tokens, f)) {
        return ListInvocation::Passthrough;
    }
    let patterns = &args[flags.rest..];
    let modules = has_flag(&flags.tokens, "m");
    match (modules, has_flag(&flags.tokens, "u"), patterns) {
        (true, true, [all]) if all == "all" => ListInvocation::ModulesUpdates,
        (true, false, [all]) if all == "all" => ListInvocation::ModulesAll,
        (false, false, _) => ListInvocation::Packages,
        _ => ListInvocation::Passthrough,
    }
}

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    let args = crate::core::args_utils::restore_double_dash(args);
    let flags = go_flags(&args, &list_takes_value);
    let chdir = flag_value(&flags.tokens, "C");
    let modfile = flag_value(&flags.tokens, "modfile");
    match classify(&args) {
        ListInvocation::Passthrough => go_mod_cmd::run_go_passthrough("list", &args, verbose),
        // Several main modules: the direct/indirect split has no single go.mod to read.
        ListInvocation::ModulesAll if go_mod_cmd::in_workspace(chdir) => {
            go_mod_cmd::run_go_passthrough("list", &args, verbose)
        }
        invocation => {
            let requires = go_mod_cmd::resolve_go_mod(chdir, modfile)
                .as_deref()
                .and_then(go_mod_cmd::read_requires);
            run_filtered_list(&args, invocation, requires, verbose)
        }
    }
}

fn run_filtered_list(
    args: &[String],
    invocation: ListInvocation,
    requires: Option<Vec<Require>>,
    verbose: u8,
) -> Result<i32> {
    let mut cmd = resolved_command("go");
    cmd.arg("list").args(args);
    if verbose > 0 {
        eprintln!("Running: go list {}", args.join(" "));
    }
    runner::run_filtered_with_exit(
        cmd,
        "go list",
        &args.join(" "),
        move |stdout, exit_code| {
            if verbose > 1 {
                eprintln!("{stdout}");
            }
            match invocation {
                ListInvocation::Packages => {
                    let (filtered, all) = filter_packages(stdout);
                    append_hint(stdout, filtered, exit_code, || {
                        (all.len() > MAX_PACKAGES + 1)
                            .then(|| {
                                tee::force_tee_tail_hint(
                                    &all.join("\n"),
                                    LIST_TEE_LABEL,
                                    1 + MAX_PACKAGES + 1,
                                )
                            })
                            .flatten()
                    })
                }
                ListInvocation::ModulesAll => {
                    let filtered = filter_modules_all(stdout, requires.as_deref());
                    append_hint(stdout, filtered, exit_code, || {
                        tee::force_tee_hint(stdout, LIST_TEE_LABEL)
                    })
                }
                ListInvocation::ModulesUpdates => {
                    let filtered = filter_modules_updates(stdout, requires.as_deref());
                    append_hint(stdout, filtered, exit_code, || {
                        tee::force_tee_hint(stdout, LIST_TEE_LABEL)
                    })
                }
                ListInvocation::Passthrough => stdout.to_string(),
            }
        },
        runner::RunOptions::stdout_only().tee(LIST_TEE_LABEL),
    )
}

/// Longest `/`-bounded prefix shared by every line (a line may equal it: the module root).
fn common_prefix<'a>(lines: &[&'a str]) -> &'a str {
    let Some(first) = lines.first() else {
        return "";
    };
    let mut prefix: &str = first;
    for line in lines {
        while !(*line == prefix || line.starts_with(&format!("{prefix}/"))) {
            match prefix.rsplit_once('/') {
                Some((shorter, _)) => prefix = shorter,
                None => return "",
            }
        }
    }
    prefix
}

/// Package list with the shared path printed once. Returns the shown text and the full
/// formatted list (header + every line) for recall.
fn filter_packages(stdout: &str) -> (String, Vec<String>) {
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    let prefix = common_prefix(&lines);
    if lines.len() < 2 || prefix.is_empty() {
        return (stdout.to_string(), Vec::new());
    }
    let mut all = vec![format!("go list: {} packages in {prefix}", lines.len())];
    all.extend(lines.iter().map(|line| {
        let relative = line
            .strip_prefix(prefix)
            .and_then(|r| r.strip_prefix('/'))
            .unwrap_or(".");
        format!("  {relative}")
    }));
    let mut shown: Vec<String> = all.iter().take(1 + MAX_PACKAGES).cloned().collect();
    if lines.len() > MAX_PACKAGES {
        shown.push(format!("  … +{} more", lines.len() - MAX_PACKAGES));
    }
    (shown.join("\n"), all)
}

fn filter_modules_all(stdout: &str, requires: Option<&[Require]>) -> String {
    let mut lines = stdout.lines().filter(|l| !l.trim().is_empty());
    let Some(main) = lines.next() else {
        return stdout.to_string();
    };
    let modules: Vec<&str> = lines.collect();
    let (header, shown): (String, Vec<&str>) = match requires {
        Some(requires) => {
            let direct: HashSet<&str> = requires
                .iter()
                .filter(|r| !r.indirect)
                .map(|r| r.path.as_str())
                .collect();
            let shown: Vec<&str> = modules
                .iter()
                .copied()
                .filter(|m| {
                    m.split_whitespace()
                        .next()
                        .is_some_and(|p| direct.contains(p))
                })
                .collect();
            let header = format!(
                "go list -m all: {} modules ({} direct, {} indirect), main {main}",
                modules.len(),
                shown.len(),
                modules.len() - shown.len()
            );
            (header, shown)
        }
        None => (
            format!("go list -m all: {} modules, main {main}", modules.len()),
            modules.clone(),
        ),
    };
    let mut out = vec![header];
    out.extend(shown.iter().take(MAX_MODULES).map(|m| format!("  {m}")));
    if shown.len() > MAX_MODULES {
        out.push(format!("  … +{} more", shown.len() - MAX_MODULES));
    }
    out.join("\n")
}

/// Modules with an available update (`[vX]`) or a `(retracted)`/`(deprecated)` marker. With a
/// readable go.mod only direct requirements are listed and indirect updates are counted:
/// bumping an indirect dependency is rarely the user's move, and the full list is in recall.
fn filter_modules_updates(stdout: &str, requires: Option<&[Require]>) -> String {
    let mut lines = stdout.lines().filter(|l| !l.trim().is_empty());
    if lines.next().is_none() {
        return stdout.to_string();
    }
    let modules: Vec<&str> = lines.collect();
    let direct: Option<HashSet<&str>> = requires.map(|requires| {
        requires
            .iter()
            .filter(|r| !r.indirect)
            .map(|r| r.path.as_str())
            .collect()
    });
    let mut updates = 0;
    let mut direct_updates = 0;
    let mut rows: Vec<String> = Vec::new();
    for line in &modules {
        let words: Vec<&str> = line.split_whitespace().collect();
        let [path, version, rest @ ..] = words.as_slice() else {
            continue;
        };
        let newer = rest
            .iter()
            .find_map(|w| w.strip_prefix('[').and_then(|w| w.strip_suffix(']')));
        let markers: Vec<&str> = rest
            .iter()
            .copied()
            .filter(|w| w.starts_with('('))
            .collect();
        if newer.is_none() && markers.is_empty() {
            continue;
        }
        let listed = direct.as_ref().is_none_or(|d| d.contains(path));
        updates += usize::from(newer.is_some());
        direct_updates += usize::from(newer.is_some() && listed);
        if !listed {
            continue;
        }
        let mut row = match newer {
            Some(newer) => format!("{path} {version} → {newer}"),
            None => format!("{path} {version}"),
        };
        for marker in markers {
            row.push(' ');
            row.push_str(marker);
        }
        rows.push(row);
    }
    if updates == 0 && rows.is_empty() {
        return format!(
            "go list -m -u all: all {} modules up to date",
            modules.len()
        );
    }
    let split = if direct.is_some() {
        format!(
            " ({direct_updates} direct, {} indirect)",
            updates - direct_updates
        )
    } else {
        String::new()
    };
    let mut out = vec![format!(
        "go list -m -u all: {updates} of {} modules have updates{split}",
        modules.len()
    )];
    out.extend(rows.iter().take(MAX_UPDATES).map(|row| format!("  {row}")));
    if rows.len() > MAX_UPDATES {
        out.push(format!("  … +{} more", rows.len() - MAX_UPDATES));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmds::go::go_mod_cmd::tests::{assert_savings, s};

    #[test]
    fn classifies_list_forms() {
        assert_eq!(classify(&s(&["./..."])), ListInvocation::Packages);
        assert_eq!(classify(&s(&[])), ListInvocation::Packages);
        assert_eq!(classify(&s(&["./...", "-json"])), ListInvocation::Packages);
        assert_eq!(
            classify(&s(&["-tags", "e2e", "./..."])),
            ListInvocation::Packages
        );
        assert_eq!(classify(&s(&["-m", "all"])), ListInvocation::ModulesAll);
        assert_eq!(
            classify(&s(&["-m", "-u", "all"])),
            ListInvocation::ModulesUpdates
        );
        for args in [
            &["-f", "{{.Dir}}", "./..."][..],
            &["-json", "./..."],
            &["-json=Dir", "./..."],
            &["-e", "./..."],
            &["-m", "golang.org/x/net"],
            &["-m", "-versions", "golang.org/x/net"],
            &["-deps", "./..."],
            &["-help"],
        ] {
            assert_eq!(classify(&s(args)), ListInvocation::Passthrough, "{args:?}");
        }
    }

    #[test]
    fn packages_print_the_module_path_once() {
        let raw = "example.com/m\nexample.com/m/a\nexample.com/m/a/b\n";
        let (out, _) = filter_packages(raw);
        assert_eq!(out, "go list: 3 packages in example.com/m\n  .\n  a\n  a/b");
    }

    #[test]
    fn packages_without_a_common_prefix_are_unchanged() {
        let raw = "a.io/x\nb.io/y\n";
        assert_eq!(filter_packages(raw).0, raw);
        assert_eq!(filter_packages("example.com/m\n").0, "example.com/m\n");
    }

    #[test]
    fn packages_are_capped_with_the_formatted_list_for_recall() {
        let raw: String = (0..60)
            .map(|i| format!("example.com/m/p{i:02}\n"))
            .collect();
        let (out, all) = filter_packages(&raw);
        assert!(out.ends_with("  … +10 more"), "{out}");
        assert_eq!(all.len(), 61);
        assert_eq!(all[0], "go list: 60 packages in example.com/m");
        assert_eq!(all[51], "  p50");
    }

    fn req(path: &str, indirect: bool) -> Require {
        Require {
            path: path.into(),
            version: "v1".into(),
            indirect,
        }
    }

    #[test]
    fn modules_all_shows_direct_requirements() {
        let raw = "example.com/m\na.io/x v1.0.0\nb.io/y v2.0.0\nc.io/z v0.1.0\n";
        let requires = [req("a.io/x", false), req("b.io/y", true)];
        assert_eq!(
            filter_modules_all(raw, Some(&requires[..])),
            "go list -m all: 3 modules (1 direct, 2 indirect), main example.com/m\n  a.io/x v1.0.0"
        );
        assert_eq!(
            filter_modules_all(raw, None),
            "go list -m all: 3 modules, main example.com/m\n  a.io/x v1.0.0\n  b.io/y v2.0.0\n  c.io/z v0.1.0"
        );
    }

    #[test]
    fn modules_updates_lists_only_updatable_modules_direct_first() {
        let raw = "\
example.com/m
a.io/x v1.0.0
b.io/y v1.0.0 [v1.2.0]
c.io/z v0.1.0 (retracted) [v0.2.0]
d.io/w v1.0.0 (deprecated)
";
        // Indirect updates are counted, not listed: bumping them is rarely the user's move.
        let requires = [req("c.io/z", false)];
        assert_eq!(
            filter_modules_updates(raw, Some(&requires[..])),
            "\
go list -m -u all: 2 of 4 modules have updates (1 direct, 1 indirect)
  c.io/z v0.1.0 → v0.2.0 (retracted)"
        );
        // Without go.mod nothing is known to be direct, so every flagged module is listed.
        assert_eq!(
            filter_modules_updates(raw, None),
            "\
go list -m -u all: 2 of 4 modules have updates
  b.io/y v1.0.0 → v1.2.0
  c.io/z v0.1.0 → v0.2.0 (retracted)
  d.io/w v1.0.0 (deprecated)"
        );
        assert_eq!(
            filter_modules_updates("example.com/m\na.io/x v1.0.0\n", None),
            "go list -m -u all: all 1 modules up to date"
        );
    }

    #[test]
    fn packages_fixture() {
        let input = include_str!("../../../tests/fixtures/go_list_packages_raw.txt");
        let (out, _) = filter_packages(input);
        assert!(out.starts_with("go list: "), "{out}");
        assert!(
            out.contains(" packages in google.golang.org/grpc\n  .\n"),
            "{out}"
        );
        assert_savings("go list ./...", input, &out);
    }

    #[test]
    fn modules_all_fixture() {
        let input = include_str!("../../../tests/fixtures/go_list_m_all_raw.txt");
        let requires =
            go_mod_cmd::parse_requires(include_str!("../../../tests/fixtures/go_list_go.mod"));
        let out = filter_modules_all(input, Some(requires.as_slice()));
        assert!(out.starts_with("go list -m all: "), "{out}");
        assert_savings("go list -m all", input, &out);
    }

    #[test]
    fn modules_updates_fixture() {
        let input = include_str!("../../../tests/fixtures/go_list_m_u_all_raw.txt");
        let requires =
            go_mod_cmd::parse_requires(include_str!("../../../tests/fixtures/go_list_go.mod"));
        let out = filter_modules_updates(input, Some(requires.as_slice()));
        assert_eq!(
            out,
            "\
go list -m -u all: 46 of 86 modules have updates (14 direct, 32 indirect)
  cloud.google.com/go/auth v0.23.1 → v0.23.3
  cloud.google.com/go/compute/metadata v0.9.0 → v0.9.1
  github.com/golang/protobuf v1.5.4 (deprecated)
  github.com/spiffe/go-spiffe/v2 v2.8.1 → v2.8.2
  go.opentelemetry.io/contrib/detectors/gcp v1.45.0 → v1.46.0
  go.opentelemetry.io/otel v1.45.0 → v1.46.0
  go.opentelemetry.io/otel/metric v1.45.0 → v1.46.0
  go.opentelemetry.io/otel/sdk v1.45.0 → v1.46.0
  go.opentelemetry.io/otel/sdk/metric v1.45.0 → v1.46.0
  go.opentelemetry.io/otel/trace v1.45.0 → v1.46.0
  golang.org/x/net v0.58.0 → v0.59.0
  golang.org/x/oauth2 v0.36.0 → v0.37.0
  golang.org/x/sync v0.22.0 → v0.23.0
  golang.org/x/sys v0.47.0 → v0.48.0
  google.golang.org/genproto/googleapis/rpc v0.0.0-20260817212433-ac3dfec99bb1 → v0.0.0-20260921155816-b14227669459"
        );
        assert_savings("go list -m -u all", input, &out);
    }
}
