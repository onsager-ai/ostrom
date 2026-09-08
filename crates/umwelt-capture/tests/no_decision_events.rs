use std::{fs, path::Path};

#[test]
fn source_trees_do_not_author_decision_events() {
    // CLAUDE.md principle 1: nothing here decides. This bars authoring decision
    // events, not forwarding a consumer's event: Sink::forward still accepts
    // any kind ethogram validates, including decisions.
    // This textual guard cannot see an event type constructed dynamically or
    // a payload built field by field. It is not a semantic Rust analysis.
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates directory");
    // Every crate in the workspace, discovered rather than listed. A hardcoded
    // pair would silently stop covering umwelt-companion the day it is added,
    // and the companion is the crate closest to the line: its permission bridge
    // transports a human's decision, and a rule or a remembered answer living
    // there would be the governor on a laptop that principle 1 forbids.
    let mut scanned = 0;
    for entry in fs::read_dir(crates).expect("read crates directory") {
        let source_tree = entry.expect("crates entry").path().join("src");
        if source_tree.is_dir() {
            scan_source_tree(&source_tree);
            scanned += 1;
        }
    }
    assert!(
        scanned >= 2,
        "expected to scan every crate's src tree, scanned {scanned}"
    );
}

fn scan_source_tree(directory: &Path) {
    for entry in fs::read_dir(directory).expect("read source directory") {
        let path = entry.expect("source entry").path();
        if path.is_dir() {
            scan_source_tree(&path);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = fs::read_to_string(&path).expect("read Rust source");
            let code = without_comments(&source);
            for forbidden in [
                "\"decision.requested\"",
                "\"decision.answered\"",
                "DECISION_REQUESTED",
                "DECISION_ANSWERED",
            ] {
                assert!(
                    !code.contains(forbidden),
                    "{} authors a forbidden decision event via {forbidden}",
                    path.display()
                );
            }
        }
    }
}

fn without_comments(source: &str) -> String {
    let mut rest = source;
    let mut code = String::new();
    while !rest.is_empty() {
        if rest.starts_with("//") {
            rest = rest.find('\n').map_or("", |end| &rest[end..]);
            code.push(' ');
        } else if let Some(after_open) = rest.strip_prefix("/*") {
            rest = after_open;
            let mut depth = 1;
            while depth > 0 && !rest.is_empty() {
                if let Some(after_open) = rest.strip_prefix("/*") {
                    depth += 1;
                    rest = after_open;
                } else if let Some(after_close) = rest.strip_prefix("*/") {
                    depth -= 1;
                    rest = after_close;
                } else {
                    rest = &rest[rest.chars().next().expect("comment character").len_utf8()..];
                }
            }
            code.push(' ');
        } else {
            // Preserve strings as a unit so a URL or comment delimiter inside
            // a literal cannot hide code that follows it. Include raw strings.
            let raw_hashes = rest
                .strip_prefix('r')
                .map(|tail| tail.bytes().take_while(|byte| *byte == b'#').count());
            let end = if let Some(hashes) = raw_hashes
                && rest.as_bytes().get(1 + hashes) == Some(&b'"')
            {
                let start = 2 + hashes;
                let closing = format!("\"{}", "#".repeat(hashes));
                rest[start..]
                    .find(&closing)
                    .map_or(rest.len(), |end| start + end + closing.len())
            } else if rest.starts_with('"') {
                let mut escaped = false;
                rest.char_indices()
                    .skip(1)
                    .find_map(|(index, character)| {
                        if escaped {
                            escaped = false;
                        } else if character == '\\' {
                            escaped = true;
                        } else if character == '"' {
                            return Some(index + 1);
                        }
                        None
                    })
                    .unwrap_or(rest.len())
            } else {
                rest.chars().next().expect("source character").len_utf8()
            };
            code.push_str(&rest[..end]);
            rest = &rest[end..];
        }
    }
    code
}

#[test]
fn comments_are_ignored_but_literals_and_constants_survive() {
    let code = without_comments(
        r###"
        // "decision.requested" DECISION_REQUESTED
        /* DECISION_ANSWERED /* nested */ "decision.answered" */
        let url = "https://example.test"; // ignored
        let raw = r#"/* not a comment */"#;
        const _X: &str = "decision.requested";
        use ethogram::DECISION_ANSWERED;
        "###,
    );
    assert!(!code.contains("DECISION_REQUESTED"));
    assert!(!code.contains("\"decision.answered\""));
    assert!(code.contains("https://example.test"));
    assert!(code.contains("/* not a comment */"));
    assert!(code.contains("\"decision.requested\""));
    assert!(code.contains("DECISION_ANSWERED"));
}
