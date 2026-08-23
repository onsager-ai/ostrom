use ostrom_core::Selector;
use regex::Regex;

pub struct SelectorCandidate<'a> {
    pub item_type: &'a str,
    pub title: &'a str,
    pub labels: &'a [String],
    pub refs: &'a [u64],
    pub files: &'a [String],
}

/// The retired selector vocabulary, as understood by the central format.
/// Anything outside it is not a selector this matcher can evaluate — callers
/// that depend on the answer being correct must check with
/// [`legacy_prefix_is_known`] rather than reading `false` as "did not match".
pub const LEGACY_SELECTOR_PREFIXES: &[&str] = &["label", "scope", "type", "path", "ref", "title"];

/// Whether a selector uses a prefix this matcher evaluates. `selector_match`
/// answers `false` both for "evaluated, did not match" and for "cannot
/// evaluate"; where that distinction matters, ask this first.
#[must_use]
pub fn legacy_prefix_is_known(selector: &str) -> bool {
    selector
        .split_once(':')
        .is_some_and(|(prefix, _)| LEGACY_SELECTOR_PREFIXES.contains(&prefix))
}

pub fn selector_match(candidate: &SelectorCandidate<'_>, selector: &Selector) -> bool {
    selector_match_str(candidate, selector.as_str())
}

pub fn selector_match_str(candidate: &SelectorCandidate<'_>, selector: &str) -> bool {
    let Some((prefix, glob)) = selector.split_once(':') else {
        return false;
    };
    let (item_type, scopes) = conventional(candidate.title);
    match prefix {
        "label" => candidate
            .labels
            .iter()
            .any(|value| glob_match(value, glob, false)),
        "scope" => scopes.iter().any(|value| glob_match(value, glob, false)),
        "type" => glob_match(&item_type, glob, false),
        "path" => {
            candidate.item_type == "pr"
                && candidate
                    .files
                    .iter()
                    .any(|value| glob_match(value, glob, true))
        }
        "ref" => candidate
            .refs
            .iter()
            .any(|number| format!("#{number}") == glob),
        "title" => glob_match(candidate.title, glob, false),
        _ => false,
    }
}

/// Compiled once. `conventional` is a selector precondition evaluated for every
/// candidate against every selector, so recompiling per call is real work in the
/// sweep's hot path for no benefit.
static CONVENTIONAL: std::sync::LazyLock<Option<Regex>> =
    std::sync::LazyLock::new(|| Regex::new(r"^([^(:\s]+)(?:\(([^)]*)\))?:").ok());

fn conventional(title: &str) -> (String, Vec<String>) {
    let Some(regex) = CONVENTIONAL.as_ref() else {
        return (String::new(), Vec::new());
    };
    let Some(captures) = regex.captures(title) else {
        return (String::new(), Vec::new());
    };
    let item_type = captures
        .get(1)
        .map_or("", |value| value.as_str())
        .to_owned();
    let scopes = captures
        .get(2)
        .map_or("", |value| value.as_str())
        .split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_owned)
        .collect();
    (item_type, scopes)
}

pub(crate) fn glob_match(value: &str, glob: &str, path: bool) -> bool {
    let mut body = String::from("^");
    let chars = glob.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '*' {
            if path && chars.get(index + 1) == Some(&'*') {
                if chars.get(index + 2) == Some(&'/') {
                    body.push_str("(?:.*/)?");
                    index += 3;
                } else {
                    body.push_str(".*");
                    index += 2;
                }
            } else {
                body.push_str(if path { "[^/]*" } else { ".*" });
                index += 1;
            }
        } else {
            body.push_str(&regex::escape(&chars[index].to_string()));
            index += 1;
        }
    }
    body.push('$');
    Regex::new(&format!("(?i:{body})")).is_ok_and(|regex| regex.is_match(value))
}

#[cfg(test)]
mod tests {
    use ostrom_core::Selector;

    use super::{SelectorCandidate, selector_match};

    #[test]
    fn path_selectors_are_pull_request_only() {
        let files = vec!["docs/guide.md".to_owned()];
        let selector = Selector::new("path:docs/**").expect("valid selector");
        let issue = SelectorCandidate {
            item_type: "issue",
            title: "docs: placeholder",
            labels: &[],
            refs: &[7],
            files: &files,
        };
        let pull_request = SelectorCandidate {
            item_type: "pr",
            ..issue
        };

        assert!(!selector_match(&issue, &selector));
        assert!(selector_match(&pull_request, &selector));
    }

    #[test]
    fn selector_preconditions_share_conventional_and_glob_semantics() {
        let labels = vec!["Needs Review".to_owned()];
        let files = vec!["crates/store/src/lib.rs".to_owned()];
        let candidate = SelectorCandidate {
            item_type: "pr",
            title: "feat(tooling, cli): placeholder",
            labels: &labels,
            refs: &[7, 42],
            files: &files,
        };

        for selector in [
            "label:needs*",
            "scope:CLI",
            "type:FEAT",
            "path:crates/**/lib.rs",
            "ref:#42",
            "title:*PLACEHOLDER",
        ] {
            let selector = Selector::new(selector).expect("valid selector");
            assert!(selector_match(&candidate, &selector), "{selector:?}");
        }
    }
}
