//! Go's command-line flag rules for the Go subcommand filters (`go_mod_cmd`, `go_list_cmd`,
//! `go_generate_cmd`): atomic single-dash names, parsing that stops at the first package
//! argument, and boolean flags that an explicit `=false` turns off.

use crate::core::arg_tokenizer::{self, Dialect, Token, TokenKind, ValueSpec};

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

/// A boolean Go flag is on unless its last occurrence carries a false value (`-diff=false`),
/// as `strconv.ParseBool` reads it.
pub(crate) fn bool_flag(tokens: &[Token<'_>], name: &str) -> bool {
    tokens
        .iter()
        .rfind(|t| t.kind == TokenKind::Long && t.text == name)
        .is_some_and(|t| {
            !matches!(
                t.attached,
                Some("0" | "f" | "F" | "false" | "FALSE" | "False")
            )
        })
}

pub(crate) fn wants_help(tokens: &[Token<'_>]) -> bool {
    has_flag(tokens, "h") || has_flag(tokens, "help")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmds::go::go_run::test_support::s;

    fn values(kind: TokenKind, name: &str) -> Option<ValueSpec> {
        (kind == TokenKind::Long && matches!(name, "C" | "modfile" | "go")).then(ValueSpec::value)
    }

    #[test]
    fn go_flags_keeps_single_dash_names_whole() {
        let args = s(&["-modfile=tools/go.mod", "-C", "sub", "-x"]);
        let flags = go_flags(&args, &values);
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
        let flags = go_flags(&args, &values);
        assert_eq!(flags.rest, 1);
        assert!(!has_flag(&flags.tokens, "json"));
        let args = s(&["-x", "--", "-json"]);
        assert_eq!(go_flags(&args, &values).rest, 2);
    }

    #[test]
    fn go_flags_treats_an_absolute_path_as_a_positional() {
        let args = s(&["/abs/dir/...", "-x"]);
        let flags = go_flags(&args, &values);
        assert_eq!(flags.rest, 0);
        assert!(flags.tokens.is_empty());
    }

    #[test]
    fn chdir_with_an_absolute_path_is_a_flag_value() {
        let args = s(&["-C", "/abs/dir", "-x"]);
        let flags = go_flags(&args, &values);
        assert_eq!(flag_value(&flags.tokens, "C"), Some("/abs/dir"));
        assert_eq!(flags.rest, 3);
    }

    #[test]
    fn flag_names_are_case_sensitive() {
        let args = s(&["-c"]);
        assert!(!has_flag(&go_flags(&args, &values).tokens, "C"));
    }

    #[test]
    fn flag_values_are_read() {
        let args = s(&["-modfile=tools/go.mod", "-C", "sub"]);
        let flags = go_flags(&args, &values);
        assert_eq!(flag_value(&flags.tokens, "modfile"), Some("tools/go.mod"));
        assert_eq!(flag_value(&flags.tokens, "C"), Some("sub"));
        assert_eq!(flag_value(&flags.tokens, "go"), None);
    }

    #[test]
    fn boolean_flags_honour_an_explicit_false() {
        let args = s(&["-diff=false", "-e"]);
        let tokens = go_flags(&args, &values).tokens;
        assert!(!bool_flag(&tokens, "diff"));
        assert!(bool_flag(&tokens, "e"));
        let args = s(&["-e=true", "-x=0"]);
        let tokens = go_flags(&args, &values).tokens;
        assert!(bool_flag(&tokens, "e"));
        assert!(!bool_flag(&tokens, "x"));
    }
}
