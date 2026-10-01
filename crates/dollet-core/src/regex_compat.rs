//! Bridging the regex dialect users' saved patterns were written in.
//!
//! Patterns reach this project from imports and from users who wrote them for
//! PCRE-flavoured engines, so they may carry lookaround and backreferences.
//! They compile with `fancy_regex`, never `regex`.
//!
//! Lives at the crate root because both the persistence layer (validating a
//! pattern on the way in) and `sync` (applying one) need it, and a pure module
//! must not reach into `db` to find it.

/// Rewrite JS-style `$1` backreferences as `\1`.
///
/// For *search* patterns only. `$` is an anchor, so `$` followed by a digit can
/// never match anything and is always a JS-ism; `fancy_regex` needs `\1` to
/// read it as a backreference. Escaped `\$` is left alone, and replacement
/// templates are left alone entirely — Rust's replacement syntax is already
/// `$1`, so "converting" one would turn a capture into the literal text `\1`.
pub fn js_backrefs_to_rust(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(c);
            if let Some(escaped) = chars.next() {
                out.push(escaped);
            }
            continue;
        }

        if c == '$' && chars.peek().is_some_and(char::is_ascii_digit) {
            out.push('\\');
            continue;
        }

        out.push(c);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_only_digit_dollars() {
        assert_eq!(js_backrefs_to_rust("(a)$1"), r"(a)\1");
        assert_eq!(js_backrefs_to_rust("$1$2"), r"\1\2");
        // A trailing `$` is an anchor, not a backreference.
        assert_eq!(js_backrefs_to_rust("^(.*)$"), "^(.*)$");
        assert_eq!(js_backrefs_to_rust(""), "");
    }

    #[test]
    fn leaves_escaped_dollars_alone() {
        assert_eq!(js_backrefs_to_rust(r"price\$1"), r"price\$1");
        assert_eq!(js_backrefs_to_rust(r"\\$1"), r"\\\1");
    }

    #[test]
    fn preserves_lookaround_for_fancy_regex() {
        let pattern = r"^(?=.*HD)(.*)$";
        assert_eq!(js_backrefs_to_rust(pattern), pattern);
        assert!(fancy_regex::Regex::new(pattern).is_ok());
    }
}
