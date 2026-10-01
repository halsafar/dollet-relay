//! Ordered include/exclude rules over a provider's stream list.
//!
//! Patterns are user-authored and PCRE-flavoured, so they compile with
//! `fancy_regex` and never `regex`: they were written for PCRE-flavoured
//! engines and may use lookaround or backreferences, which Rust's `regex` crate
//! rejects by design. A pattern that will not compile is reported rather than
//! dropped quietly — a filter that silently stops applying changes which
//! streams exist, which is indistinguishable from the provider changing.

use crate::regex_compat::js_backrefs_to_rust;

/// Which part of a stream a rule matches against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterTarget {
    Name,
    Group,
    Url,
}

#[derive(Debug, Clone)]
pub struct StreamFilter {
    pub target: FilterTarget,
    pub pattern: String,
    /// True excludes what it matches, false admits only what it matches.
    pub exclude: bool,
    /// Matching is case-*sensitive* unless the rule's
    /// `custom_properties.case_sensitive` is explicitly `false`.
    pub case_sensitive: bool,
}

/// A pattern that would not compile, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct FilterError {
    /// Index into the slice handed to [`compile`].
    pub index: usize,
    pub pattern: String,
    pub message: String,
}

pub struct CompiledFilters {
    rules: Vec<(fancy_regex::Regex, FilterTarget, bool)>,
}

/// Compile a rule set, in the order it should be applied.
///
/// Returns the rules that compiled and the errors for those that did not, so a
/// caller can apply the working ones and still report the broken.
pub fn compile(filters: &[StreamFilter]) -> (CompiledFilters, Vec<FilterError>) {
    let mut rules = Vec::with_capacity(filters.len());
    let mut errors = Vec::new();

    for (index, filter) in filters.iter().enumerate() {
        // `$1` in a *search* pattern can only be a JS-ism: `$` is an anchor, so
        // `$` followed by a digit never matches anything.
        let normalised = js_backrefs_to_rust(&filter.pattern);
        let source = if filter.case_sensitive {
            normalised
        } else {
            format!("(?i){normalised}")
        };
        match fancy_regex::Regex::new(&source) {
            Ok(regex) => rules.push((regex, filter.target, filter.exclude)),
            Err(error) => errors.push(FilterError {
                index,
                pattern: filter.pattern.clone(),
                message: error.to_string(),
            }),
        }
    }

    (CompiledFilters { rules }, errors)
}

impl CompiledFilters {
    /// Whether a stream survives the rule set.
    ///
    /// The **first** rule that matches decides, include or exclude; a stream no
    /// rule matches is admitted. That ordering is the whole semantics — an
    /// include rule after a broad exclude is dead, and users rely on putting the
    /// specific case first.
    pub fn admits(&self, name: &str, url: &str, group: &str) -> bool {
        for (regex, target, exclude) in &self.rules {
            let subject = match target {
                FilterTarget::Name => name,
                FilterTarget::Group => group,
                FilterTarget::Url => url,
            };
            // A backtracking engine can fail on a pathological input rather than
            // just not matching. Treat that as "did not match" and keep going:
            // one bad input must not drop every remaining rule.
            if regex.is_match(subject).unwrap_or(false) {
                return !exclude;
            }
        }
        true
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(target: FilterTarget, pattern: &str, exclude: bool) -> StreamFilter {
        StreamFilter {
            target,
            pattern: pattern.into(),
            exclude,
            case_sensitive: true,
        }
    }

    fn admits(filters: &[StreamFilter], name: &str, url: &str, group: &str) -> bool {
        let (compiled, errors) = compile(filters);
        assert_eq!(errors, Vec::new());
        compiled.admits(name, url, group)
    }

    #[test]
    fn a_stream_no_rule_matches_is_admitted() {
        let rules = [filter(FilterTarget::Name, "^Sports", true)];
        assert!(admits(&rules, "News at Ten", "http://x/a", "News"));
        assert!(admits(&[], "anything", "http://x/a", "News"));
        assert!(compile(&[]).0.is_empty());
    }

    #[test]
    fn each_target_reads_its_own_field() {
        let cases: &[(FilterTarget, bool)] = &[
            (FilterTarget::Name, true),
            (FilterTarget::Group, false),
            (FilterTarget::Url, false),
        ];
        for (target, matched) in cases {
            let rules = [filter(*target, "Sports", true)];
            // Only the name contains "Sports".
            assert_eq!(
                !admits(&rules, "Sports HD", "http://x/a", "News"),
                *matched,
                "{target:?}"
            );
        }

        assert!(!admits(
            &[filter(FilterTarget::Group, "Adult", true)],
            "A",
            "http://x/a",
            "Adult"
        ));
        assert!(!admits(
            &[filter(FilterTarget::Url, "backup", true)],
            "A",
            "http://backup/a",
            "News"
        ));
    }

    #[test]
    fn the_first_matching_rule_decides() {
        // An include placed before a broad exclude is how a user keeps one
        // channel out of a group they are otherwise dropping.
        let rules = [
            filter(FilterTarget::Name, "^Sports HD$", false),
            filter(FilterTarget::Name, "^Sports", true),
        ];
        assert!(admits(&rules, "Sports HD", "http://x/a", "News"));
        assert!(!admits(&rules, "Sports SD", "http://x/a", "News"));

        // Reversed, the include is dead.
        let reversed = [
            filter(FilterTarget::Name, "^Sports", true),
            filter(FilterTarget::Name, "^Sports HD$", false),
        ];
        assert!(!admits(&reversed, "Sports HD", "http://x/a", "News"));
    }

    #[test]
    fn an_include_rule_admits_only_what_it_matches() {
        // A lone include still admits everything it does not match, because
        // "no rule matched" is a pass. That surprises people, so it is pinned
        // here.
        let rules = [filter(FilterTarget::Name, "^Keep", false)];
        assert!(admits(&rules, "Keep This", "http://x/a", "News"));
        assert!(admits(&rules, "Unmatched", "http://x/a", "News"));
    }

    #[test]
    fn matching_is_a_search_not_an_anchored_match() {
        let rules = [filter(FilterTarget::Name, "Sports", true)];
        assert!(!admits(
            &rules,
            "Local Sports Network",
            "http://x/a",
            "News"
        ));
    }

    #[test]
    fn case_sensitivity_follows_the_rule_not_the_engine_default() {
        let sensitive = [filter(FilterTarget::Name, "sports", true)];
        assert!(admits(&sensitive, "Sports HD", "http://x/a", "News"));

        let insensitive = [StreamFilter {
            case_sensitive: false,
            ..filter(FilterTarget::Name, "sports", true)
        }];
        assert!(!admits(&insensitive, "Sports HD", "http://x/a", "News"));
    }

    #[test]
    fn lookaround_and_backreferences_compile() {
        // The reason for `fancy_regex`: Rust's `regex` crate rejects both.
        let rules = [
            filter(FilterTarget::Name, r"^(?!Keep)", true),
            filter(FilterTarget::Group, r"(\w+) \1", true),
        ];
        let (compiled, errors) = compile(&rules);
        assert_eq!(errors, Vec::new());
        assert!(compiled.admits("Keep This", "http://x/a", "News"));
        assert!(!compiled.admits("Drop This", "http://x/a", "News"));
        assert!(!compiled.admits("Keep This", "http://x/a", "echo echo"));
    }

    #[test]
    fn a_js_backreference_in_a_pattern_is_rewritten() {
        // `$` is an anchor, so `$1` in a search pattern can only ever have been
        // written for a JS engine.
        let rules = [filter(FilterTarget::Name, r"(\w+) $1", true)];
        let (compiled, errors) = compile(&rules);
        assert_eq!(errors, Vec::new());
        assert!(!compiled.admits("echo echo", "http://x/a", "News"));
        // "echo other" would match on the single-character capture "o", so the
        // negative case needs a name sharing no boundary character.
        assert!(compiled.admits("alpha beta", "http://x/a", "News"));
    }

    #[test]
    fn an_uncompilable_pattern_is_reported_and_the_rest_still_apply() {
        let rules = [
            filter(FilterTarget::Name, "(unclosed", true),
            filter(FilterTarget::Name, "^Sports", true),
        ];
        let (compiled, errors) = compile(&rules);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].index, 0);
        assert_eq!(errors[0].pattern, "(unclosed");
        assert!(!errors[0].message.is_empty());

        // The working rule is unaffected.
        assert!(!compiled.admits("Sports HD", "http://x/a", "News"));
        assert!(compiled.admits("News", "http://x/a", "News"));
    }

    #[test]
    fn an_empty_pattern_matches_everything() {
        // Degenerate but legal, and it means the rule swallows the account.
        let rules = [filter(FilterTarget::Name, "", true)];
        assert!(!admits(&rules, "anything", "http://x/a", "News"));
    }

    #[test]
    fn a_pathological_pattern_does_not_drop_the_rules_after_it() {
        // `fancy_regex` gives up with an error rather than hanging. Treating
        // that as "did not match" keeps the remaining rules alive.
        let rules = [
            filter(FilterTarget::Name, r"(a+)+$", true),
            filter(FilterTarget::Group, "Adult", true),
        ];
        let (compiled, errors) = compile(&rules);
        assert_eq!(errors, Vec::new());

        let pathological = "a".repeat(60) + "!";
        assert!(compiled.admits(&pathological, "http://x/a", "News"));
        assert!(!compiled.admits(&pathological, "http://x/a", "Adult"));
    }
}
