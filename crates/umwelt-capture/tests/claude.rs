use std::fs;
use std::path::Path;

use ethogram::{AGENT_COMPLETED, AGENT_TEXT, MAX_EXCERPT_SCALARS, MAX_TEXT_SCALARS, parse_event};
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
fn subagent_cost_survives_json_parsing_and_golden_serialisation() {
    let raw = fs::read_to_string(Path::new(FIXTURES).join("subagent/raw.ndjson"))
        .expect("read subagent capture");
    let mut normaliser = ClaudeNormaliser::new();
    let mut drafts = Vec::new();
    for line in raw.lines() {
        drafts.extend(normaliser.line(line).expect("normalise subagent frame"));
    }
    drafts.extend(normaliser.finish().expect("finish subagent capture"));

    let costs: Vec<f64> = drafts
        .iter()
        .filter(|draft| draft.event_type == AGENT_COMPLETED)
        .map(|draft| {
            draft.payload["costUsd"]
                .as_f64()
                .expect("agent.completed costUsd")
        })
        .collect();
    // serde_json's default float parser is one ULP low for this value.
    // Ethogram requires float_roundtrip; losing that dependency feature would
    // silently rewrite money before the normaliser's value reached this fixture.
    assert_eq!(costs, vec![0.09765190000000001; 2]);

    let expected = fs::read_to_string(Path::new(FIXTURES).join("subagent/expected.jsonl"))
        .expect("read subagent golden");
    let completed: Vec<&str> = expected
        .lines()
        .filter(|line| line.contains("\"type\":\"agent.completed\""))
        .collect();
    assert_eq!(completed.len(), 2);
    assert!(
        completed
            .iter()
            .all(|line| line.contains("\"costUsd\":0.09765190000000001"))
    );
}

#[test]
fn every_seeded_ethogram_corpus_fixture_matches_our_mapped_fields() {
    const CORRESPONDING_EVENTS: [(&str, &str, usize); 9] = [
        ("agent-completed-repeated-terminal.json", "subagent", 14),
        ("agent-completed.json", "subagent", 13),
        ("agent-started.json", "subagent", 1),
        ("agent-text-truncated.json", "overbound", 2),
        ("agent-text.json", "subagent", 4),
        ("agent-tool-result-subagent.json", "subagent", 9),
        ("agent-tool-result.json", "subagent", 6),
        ("agent-tool-use-subagent.json", "subagent", 8),
        ("agent-tool-use.json", "subagent", 5),
    ];

    let fixtures = ethogram_corpus::v1_fixtures();
    assert_eq!(
        fixtures.len(),
        CORRESPONDING_EVENTS.len(),
        "the ethogram corpus inventory changed; map and review every new fixture"
    );

    for fixture in fixtures {
        let (_, case, line_number) = CORRESPONDING_EVENTS
            .iter()
            .find(|(name, _, _)| *name == fixture.name)
            .unwrap_or_else(|| panic!("unmapped ethogram fixture {:?}", fixture.name));
        let expected = fs::read_to_string(Path::new(FIXTURES).join(case).join("expected.jsonl"))
            .expect("read corresponding umwelt fixture");
        let ours = parse_event(
            expected
                .lines()
                .nth(line_number - 1)
                .expect("corresponding umwelt event line"),
        )
        .expect("parse corresponding umwelt event");
        let upstream = fixture.parse().expect("parse ethogram fixture");

        assert_eq!(ours.event_type, upstream.event_type, "{}", fixture.name);
        let ours = ours.payload.as_object().expect("umwelt payload object");
        let upstream = upstream
            .payload
            .as_object()
            .expect("ethogram payload object");
        for (field, value) in upstream {
            // Ethogram's immutable fixtures came from chreode and therefore
            // carry its stage plus a model on completions. Umwelt's ruled
            // mapping emits neither; sink-owned envelope stamps also differ.
            // Every field shared by the two producer mappings must agree.
            if field == "stage" || (field == "model" && ours.contains_key("costUsd")) {
                continue;
            }
            assert_eq!(
                ours.get(field),
                Some(value),
                "{} payload field {field:?}",
                fixture.name
            );
        }
    }
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
