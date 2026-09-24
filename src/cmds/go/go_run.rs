//! Execution helpers shared by the Go subcommand filters: tracked passthrough for the forms
//! they leave alone, and the exit-0 recovery hint for a summarised output.

use crate::core::guard::never_worse;
use crate::core::runner;
use anyhow::Result;
use std::ffi::OsString;

pub(crate) fn run_go_passthrough(sub: &str, args: &[String], verbose: u8) -> Result<i32> {
    let os_args: Vec<OsString> = std::iter::once(OsString::from(sub))
        .chain(args.iter().map(OsString::from))
        .collect();
    runner::run_passthrough("go", &os_args, verbose)
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

#[cfg(test)]
pub(crate) mod test_support {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
