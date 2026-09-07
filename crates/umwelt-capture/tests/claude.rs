use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use ethogram::{AGENT_TEXT, MAX_EXCERPT_SCALARS, MAX_TEXT_SCALARS, parse_event};
use serde_json::json;
use umwelt_capture::Normaliser;
use umwelt_capture::claude::ClaudeNormaliser;
use umwelt_capture::golden::walk_corpus;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude");

#[test]
fn corpus_matches_for_file_and_in_memory_sources_and_refuses_unknown_types() {
    let report = walk_corpus(ClaudeNormaliser::new, FIXTURES).expect("Claude corpus must match");
    assert_eq!(report.cases, 2);
    assert_eq!(report.refusals, 1);
}

#[test]
fn overbound_text_is_scalar_bounded_and_has_no_synthetic_completion() {
    let expected = fs::read_to_string(Path::new(FIXTURES).join("overbound/expected.jsonl"))
        .expect("read overbound golden");
    let events: Vec<_> = expected
        .lines()
        .map(|line| parse_event(line).expect("parse expected event"))
        .collect();
    assert_eq!(events.len(), 2, "EOF must not invent agent.completed");
    assert_eq!(events[1].event_type, AGENT_TEXT);
    assert_eq!(
        events[1].payload["text"]
            .as_str()
            .expect("agent.text text")
            .chars()
            .count(),
        MAX_TEXT_SCALARS
    );
    assert_eq!(events[1].payload["truncated"], true);
}

#[test]
fn every_narration_route_is_bounded() {
    let mut normaliser = ClaudeNormaliser::new();
    let session = "bounded-session";
    normaliser
        .line(
            &json!({
                "type": "system",
                "subtype": "init",
                "session_id": session,
                "model": "claude-test"
            })
            .to_string(),
        )
        .expect("normalise init");

    let text = "x".repeat(MAX_TEXT_SCALARS + 1);
    let draft = normaliser
        .line(
            &json!({
                "type": "assistant",
                "session_id": session,
                "message": {"content": [{"type": "text", "text": text}]}
            })
            .to_string(),
        )
        .expect("normalise text")
        .remove(0);
    assert_bounded(&draft.payload, "text", MAX_TEXT_SCALARS);

    let input = "x".repeat(MAX_EXCERPT_SCALARS + 1);
    let draft = normaliser
        .line(
            &json!({
                "type": "assistant",
                "session_id": session,
                "message": {"content": [{
                    "type": "tool_use",
                    "id": "bounded-tool",
                    "name": "Test",
                    "input": {"value": input}
                }]}
            })
            .to_string(),
        )
        .expect("normalise tool use")
        .remove(0);
    assert_bounded(&draft.payload, "inputExcerpt", MAX_EXCERPT_SCALARS);

    let result = "x".repeat(MAX_EXCERPT_SCALARS + 1);
    let draft = normaliser
        .line(
            &json!({
                "type": "user",
                "session_id": session,
                "message": {"content": [{
                    "type": "tool_result",
                    "tool_use_id": "bounded-tool",
                    "content": result
                }]}
            })
            .to_string(),
        )
        .expect("normalise tool result")
        .remove(0);
    assert_bounded(&draft.payload, "resultExcerpt", MAX_EXCERPT_SCALARS);
}

#[test]
fn ethogram_corpus_fixtures_are_byte_identical_and_unseeded_count_is_explicit() {
    let fixtures = ethogram_corpus::v1_fixtures();
    let ours = expected_lines();

    for fixture in fixtures {
        let line = fixture
            .raw_json
            .strip_suffix('\n')
            .unwrap_or(fixture.raw_json);
        assert!(
            ours.contains(line.as_bytes()),
            "ethogram fixture {:?} does not match any umwelt expected.jsonl line",
            fixture.name
        );
    }

    eprintln!(
        "ethogram conformance/v1 fixture count: {}; zero means the corpus is unseeded, not that the cross-check passed",
        fixtures.len()
    );
    assert_eq!(
        fixtures.len(),
        0,
        "ethogram conformance/v1 is now seeded; review the matches above and replace the explicit unseeded assertion"
    );
}

fn expected_lines() -> HashSet<Vec<u8>> {
    ["subagent", "overbound"]
        .into_iter()
        .flat_map(|case| {
            let path: PathBuf = Path::new(FIXTURES).join(case).join("expected.jsonl");
            fs::read(path)
                .expect("read expected fixture")
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(<[u8]>::to_vec)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn assert_bounded(payload: &serde_json::Value, field: &str, bound: usize) {
    assert_eq!(
        payload[field]
            .as_str()
            .expect("bounded payload field")
            .chars()
            .count(),
        bound
    );
    assert_eq!(payload["truncated"], true);
}
