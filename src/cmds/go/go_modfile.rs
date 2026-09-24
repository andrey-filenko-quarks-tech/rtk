//! `go.mod` knowledge for the Go subcommand filters: its `require` directives, which file a
//! command works on (`-C`, `-modfile`, `GOFLAGS`, walking up as Go does), and workspace mode.

use std::path::{Path, PathBuf};

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

/// The `go.mod` a command run with `-C chdir` / `-modfile modfile` works on; without
/// `-modfile` on the command line, one set in `GOFLAGS` applies, as it does for `go`.
pub(crate) fn resolve_go_mod(chdir: Option<&str>, modfile: Option<&str>) -> Option<PathBuf> {
    let goflags = std::env::var("GOFLAGS").ok();
    let modfile = modfile.or_else(|| goflags.as_deref().and_then(modfile_from_goflags));
    resolve_go_mod_from(&std::env::current_dir().ok()?, chdir, modfile)
}

/// The `-modfile=…` entry of a `GOFLAGS` value (space-separated `-flag=value` entries).
fn modfile_from_goflags(goflags: &str) -> Option<&str> {
    goflags.split_whitespace().find_map(|flag| {
        flag.strip_prefix("--modfile=")
            .or_else(|| flag.strip_prefix("-modfile="))
    })
}

/// `-C` is applied first (relative to `cwd`, or absolute), then `-modfile` relative to it.
fn resolve_go_mod_from(cwd: &Path, chdir: Option<&str>, modfile: Option<&str>) -> Option<PathBuf> {
    let base = chdir.map_or_else(|| cwd.to_path_buf(), |dir| cwd.join(dir));
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

/// Workspace mode: `GOWORK` names a file, or (unless `GOWORK=off`) a `go.work` sits above.
pub(crate) fn in_workspace(chdir: Option<&str>) -> bool {
    std::env::current_dir().is_ok_and(|cwd| {
        let base = chdir.map_or_else(|| cwd.clone(), |dir| cwd.join(dir));
        in_workspace_with(std::env::var("GOWORK").ok().as_deref(), &base)
    })
}

fn in_workspace_with(gowork: Option<&str>, base: &Path) -> bool {
    match gowork {
        Some("off") => false,
        Some(path) if !path.is_empty() => true,
        _ => base.ancestors().any(|dir| dir.join("go.work").is_file()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(find_go_mod(Path::new("/")), None);
    }

    #[test]
    fn resolves_go_mod_from_chdir_and_modfile() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("go.mod"), "module m\n").expect("write");
        std::fs::create_dir_all(dir.path().join("sub")).expect("mkdir");
        let cwd = dir.path();
        assert_eq!(
            resolve_go_mod_from(cwd, Some("sub"), None),
            Some(cwd.join("go.mod"))
        );
        assert_eq!(
            resolve_go_mod_from(cwd, Some("sub"), Some("tools.mod")),
            Some(cwd.join("sub/tools.mod"))
        );
        assert_eq!(
            resolve_go_mod_from(cwd, Some("/abs/elsewhere"), Some("x.mod")),
            Some(PathBuf::from("/abs/elsewhere/x.mod"))
        );
    }

    #[test]
    fn goflags_can_carry_the_modfile() {
        assert_eq!(
            modfile_from_goflags("-mod=mod -modfile=tools/go.mod"),
            Some("tools/go.mod")
        );
        assert_eq!(modfile_from_goflags("--modfile=x.mod"), Some("x.mod"));
        assert_eq!(modfile_from_goflags("-mod=mod -trimpath"), None);
        assert_eq!(modfile_from_goflags(""), None);
    }

    #[test]
    fn workspace_detection_follows_gowork() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path();
        assert!(!in_workspace_with(None, base));
        assert!(!in_workspace_with(Some("off"), base));
        assert!(in_workspace_with(Some("/some/go.work"), base));
        std::fs::write(base.join("go.work"), "go 1.22\n").expect("write");
        assert!(in_workspace_with(None, &base.join("a/b")));
        assert!(!in_workspace_with(Some("off"), base));
    }
}
