use std::fs;
use std::path::Path;

use ethogram::{
    AGENT_COMPLETED, AGENT_STARTED, AGENT_TEXT, AGENT_TOOL_RESULT, AGENT_TOOL_USE, AGENT_WARNING,
    CONTROL_APPLIED, CONTROL_REQUESTED, ControlAppliedPayload, ControlAppliedReason, ControlKind,
    ControlRequestedPayload, MAX_EXCERPT_SCALARS, MAX_TEXT_SCALARS, RUN_FINISHED,
    RunFinishedPayload, RunOutcome, parse_event, validate,
};
use serde_json::json;
use umwelt_capture::claude::ClaudeNormaliser;
use umwelt_capture::golden::{read_metadata, walk_corpus};
use umwelt_capture::{CaptureFault, Normaliser};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude");

/// Why a corpus fixture is not something umwelt can produce. Each arm is
/// asserted against the fixture below, so an entry cannot be parked here
/// merely to make the inventory assertion pass.
#[derive(Clone, Copy, PartialEq)]
enum NotOurs {
    /// Principle 1: nothing here decides.
    Decides,
    /// Principle 5: two verbs, honestly. umwelt offers `interrupt` and
    /// `steer`; `answer` is a verb it does not offer, and `interrupt()` and
    /// `steer()` refuse any other kind with `WrongKind`.
    VerbNotOffered,
    /// umwelt emits no `run.started` at all — the name appears nowhere in
    /// its source. The assertion below checks that premise rather than
    /// trusting it, so the day umwelt does emit one, this fails.
    NotEmitted,
}

// CLAUDE.md principle 1: nothing here decides. These decision events belong
// to the governor, now and later: dossier, blastRadius, optionsRuledOut,
// recommendedAction, and budget/gate_inconclusive/tripwire kinds express
// classifications, gates or verdicts that umwelt must never produce.
//
// `answer` is a control kind umwelt never requests or applies (principle 5:
// `interrupt` and `steer` are the only two verbs offered), and `run.started`
// is an event type umwelt's source never names at all.
const NOT_PRODUCED_HERE: [(&str, NotOurs); 15] = [
    ("decision-answered-excuse.json", NotOurs::Decides),
    ("decision-requested-budget.json", NotOurs::Decides),
    (
        "decision-requested-gate-inconclusive.json",
        NotOurs::Decides,
    ),
    (
        "decision-requested-human-decides-options.json",
        NotOurs::Decides,
    ),
    ("decision-requested-human-decides.json", NotOurs::Decides),
    ("decision-requested-tripwire.json", NotOurs::Decides),
    ("decision-requested-unclassified.json", NotOurs::Decides),
    (
        "decision-requested-unexplained-write.json",
        NotOurs::Decides,
    ),
    (
        "decision-answered-excuse-requested-run.json",
        NotOurs::Decides,
    ),
    ("decision-answered-permission.json", NotOurs::Decides),
    (
        "decision-answered-permission-timeout.json",
        NotOurs::Decides,
    ),
    ("decision-requested-permission.json", NotOurs::Decides),
    ("control-requested-answer.json", NotOurs::VerbNotOffered),
    ("control-applied-answer.json", NotOurs::VerbNotOffered),
    ("run-started-handoff.json", NotOurs::NotEmitted),
];

#[test]
fn corpus_matches_for_file_and_in_memory_sources_and_refuses_unknown_types() {
    let report = walk_corpus(ClaudeNormaliser::new, FIXTURES).expect("Claude corpus must match");
    assert_eq!(report.cases, 3);
    assert_eq!(report.refusals, 1);
}

#[test]
fn captured_interrupt_pins_control_answers_before_the_terminal() {
    const RUN_ID: &str = "capture-control-interrupt-fixture";
    const TOOL_USE_ID: &str = "toolu_01BG5x9iuSVKzZhYg7qmM5PJ";

    let case = Path::new(FIXTURES).join("control-interrupt");
    let metadata = read_metadata(case.join("meta.toml")).expect("read control capture metadata");
    assert_eq!(metadata.cli_version, "2.1.263");
    let raw = fs::read_to_string(case.join("raw.ndjson")).expect("read control raw capture");
    assert_eq!(raw.lines().count(), 8, "the complete raw tee must be kept");
    let events = fs::read_to_string(case.join("events.jsonl"))
        .expect("read control event capture")
        .lines()
        .map(|line| parse_event(line).expect("parse control capture event"))
        .collect::<Vec<_>>();

    assert_eq!(events.len(), 7);
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        [
            AGENT_STARTED,
            AGENT_TOOL_USE,
            CONTROL_REQUESTED,
            CONTROL_REQUESTED,
            CONTROL_APPLIED,
            CONTROL_APPLIED,
            RUN_FINISHED,
        ]
    );
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.run_id, RUN_ID);
        assert_eq!(event.seq, u64::try_from(index).expect("fixture index") + 1);
    }
    assert_eq!(events[1].payload["toolUseId"], TOOL_USE_ID);

    let steer_request: ControlRequestedPayload =
        serde_json::from_value(events[2].payload.clone()).expect("steer request payload");
    assert_eq!(steer_request.control_id, "capture-steer-1");
    assert_eq!(steer_request.kind, ControlKind::Steer);

    let interrupt_request: ControlRequestedPayload =
        serde_json::from_value(events[3].payload.clone()).expect("interrupt request payload");
    assert_eq!(interrupt_request.control_id, "capture-interrupt-1");
    assert_eq!(interrupt_request.kind, ControlKind::Interrupt);

    let interrupt_applied: ControlAppliedPayload =
        serde_json::from_value(events[4].payload.clone()).expect("interrupt answer payload");
    assert_eq!(interrupt_applied.control_id, "capture-interrupt-1");
    assert!(interrupt_applied.ok);
    assert_eq!(interrupt_applied.landed_in.as_deref(), Some(TOOL_USE_ID));

    let steer_applied: ControlAppliedPayload =
        serde_json::from_value(events[5].payload.clone()).expect("steer answer payload");
    assert_eq!(steer_applied.control_id, "capture-steer-1");
    assert!(!steer_applied.ok);
    assert_eq!(steer_applied.reason, Some(ControlAppliedReason::NotLive));

    let finished: RunFinishedPayload =
        serde_json::from_value(events[6].payload.clone()).expect("terminal payload");
    assert_eq!(finished.outcome, RunOutcome::Interrupted);
}

#[test]
fn captured_error_max_turns_emits_completion_totals_then_pinned_warning() {
    assert_non_success_result(
        "error_max_turns",
        "claude result subtype \"error_max_turns\"",
    );
}

#[test]
fn uncaptured_error_during_execution_is_mapped_without_an_allowlist() {
    assert_non_success_result(
        "error_during_execution",
        "claude result subtype \"error_during_execution\"",
    );
}

#[test]
fn non_success_result_missing_a_total_is_refused_with_the_field_name() {
    let mut normaliser = initialised_normaliser();
    let fault = normaliser
        .line(
            &json!({
                "type": "result",
                "subtype": "error_during_execution",
                "session_id": "result-session",
                "total_cost_usd": 0.0223935,
                "usage": {
                    "input_tokens": 9,
                    "output_tokens": 193,
                    "cache_read_input_tokens": 13615,
                    "cache_creation_input_tokens": 9517
                },
                "num_turns": 2
            })
            .to_string(),
        )
        .expect_err("a missing duration total must be refused");

    assert_eq!(
        fault,
        CaptureFault::MalformedLine {
            line: 2,
            reason: "missing required field result.duration_ms".to_owned(),
        }
    );
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
    let drafts = normalise_fixture("subagent");

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
fn tool_result_is_error_true_is_preserved() {
    let drafts = normalise_fixture("error-shapes");
    let result = tool_result(&drafts, "toolu_01VL7aVtfw8YsTXwszEqRDbW");
    assert_eq!(result.payload.get("isError"), Some(&json!(true)));
}

#[test]
fn tool_result_is_error_false_is_preserved() {
    let drafts = normalise_fixture("subagent");
    let result = tool_result(&drafts, "toolu_01GvYvaZZx61VzJyHo1mkUpk");
    assert_eq!(result.payload.get("isError"), Some(&json!(false)));
}

#[test]
fn absent_tool_result_is_error_stays_absent() {
    let drafts = normalise_fixture("subagent");
    for tool_use_id in [
        "toolu_01XCdbg7LeoBKasN9qNFddKa",
        "toolu_01N4UBnESypVtuFSMQMNGTG8",
    ] {
        let result = tool_result(&drafts, tool_use_id);
        assert!(
            result.payload.get("isError").is_none(),
            "absent is_error must not become false for {tool_use_id}"
        );
    }
}

#[test]
fn every_seeded_ethogram_corpus_fixture_matches_our_mapped_fields() {
    // Normaliser-derived cases carry expected.jsonl; the control case carries
    // events.jsonl because its events come from the runtime, not any raw line.
    const CORRESPONDING_EVENTS: [(&str, &str, &str, usize); 16] = [
        (
            "agent-completed-max-turns.json",
            "error-shapes",
            "expected.jsonl",
            5,
        ),
        (
            "agent-completed-repeated-terminal.json",
            "subagent",
            "expected.jsonl",
            14,
        ),
        ("agent-completed.json", "subagent", "expected.jsonl", 13),
        ("agent-started.json", "subagent", "expected.jsonl", 1),
        (
            "agent-text-truncated.json",
            "overbound",
            "expected.jsonl",
            2,
        ),
        ("agent-text.json", "subagent", "expected.jsonl", 4),
        (
            "agent-tool-result-error.json",
            "error-shapes",
            "expected.jsonl",
            4,
        ),
        (
            "agent-tool-result-subagent.json",
            "subagent",
            "expected.jsonl",
            9,
        ),
        ("agent-tool-result.json", "subagent", "expected.jsonl", 6),
        (
            "agent-tool-use-subagent.json",
            "subagent",
            "expected.jsonl",
            8,
        ),
        ("agent-tool-use.json", "subagent", "expected.jsonl", 5),
        ("agent-warning.json", "error-shapes", "expected.jsonl", 6),
        (
            "control-requested-steer.json",
            "control-interrupt",
            "events.jsonl",
            3,
        ),
        (
            "control-requested-interrupt.json",
            "control-interrupt",
            "events.jsonl",
            4,
        ),
        (
            "control-applied-interrupt.json",
            "control-interrupt",
            "events.jsonl",
            5,
        ),
        (
            "control-applied-not-live.json",
            "control-interrupt",
            "events.jsonl",
            6,
        ),
    ];

    let fixtures = ethogram::v1_fixtures();
    assert_eq!(
        fixtures.len(),
        CORRESPONDING_EVENTS.len() + NOT_PRODUCED_HERE.len(),
        "the ethogram corpus inventory changed; map and review every new fixture"
    );

    // The `control.applied` half of the answer exchange is pinned to the
    // `control.requested` half also exempted here, by controlId, rather than
    // to a file name — so look the id up instead of hardcoding it.
    let answer_control_ids: Vec<String> = NOT_PRODUCED_HERE
        .iter()
        .filter(|(_, reason)| *reason == NotOurs::VerbNotOffered)
        .filter_map(|(name, _)| {
            let fixture = fixtures
                .iter()
                .find(|fixture| fixture.name == *name)
                .unwrap_or_else(|| panic!("missing exempted ethogram fixture {name:?}"));
            let event = fixture.parse().expect("parse exempted ethogram fixture");
            (event.event_type == CONTROL_REQUESTED).then(|| {
                event.payload["controlId"]
                    .as_str()
                    .expect("control.requested controlId")
                    .to_owned()
            })
        })
        .collect();

    for (name, reason) in NOT_PRODUCED_HERE {
        let fixture = fixtures
            .iter()
            .find(|fixture| fixture.name == name)
            .unwrap_or_else(|| panic!("missing exempted ethogram fixture {name:?}"));
        let event = fixture.parse().expect("parse exempted ethogram fixture");
        match reason {
            NotOurs::Decides => {
                assert!(
                    event.event_type.starts_with("decision."),
                    "exempted fixture {name:?} must have a decision. event type, got {:?}",
                    event.event_type
                );
            }
            NotOurs::VerbNotOffered => {
                if event.event_type == CONTROL_REQUESTED {
                    assert_eq!(
                        event.payload["kind"], "answer",
                        "exempted fixture {name:?} must request the \"answer\" control kind"
                    );
                } else if event.event_type == CONTROL_APPLIED {
                    let control_id = event.payload["controlId"]
                        .as_str()
                        .expect("control.applied controlId");
                    assert!(
                        answer_control_ids.iter().any(|id| id == control_id),
                        "exempted fixture {name:?} must apply a controlId requested by a \
                         control.requested fixture also exempted as VerbNotOffered, got {control_id:?}"
                    );
                } else {
                    panic!(
                        "exempted fixture {name:?} tagged VerbNotOffered must be \
                         control.requested or control.applied, got {:?}",
                        event.event_type
                    );
                }
            }
            NotOurs::NotEmitted => {
                assert_eq!(
                    event.event_type, "run.started",
                    "exempted fixture {name:?} must be run.started"
                );
            }
        }
    }

    // umwelt emits no run.started at all — the name appears nowhere in its
    // source. Check that premise directly rather than trusting it, so the
    // day umwelt does emit one, this fails instead of staying silently
    // stale. Deliberately crude (a plain substring scan, no comment
    // stripper): its only job is to fire if the premise stops holding.
    for path in rust_source_files(&umwelt_crates_root()) {
        let contents = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        assert!(
            !contents.contains("RUN_STARTED") && !contents.contains("run.started"),
            "{} names run.started; umwelt does not emit it (see NotOurs::NotEmitted) — \
             update the exemption if that has changed",
            path.display()
        );
    }

    let mut compared = 0;
    for fixture in fixtures {
        if NOT_PRODUCED_HERE
            .iter()
            .any(|(name, _)| *name == fixture.name)
        {
            continue;
        }
        let (_, case, source_file, line_number) = CORRESPONDING_EVENTS
            .iter()
            .find(|(name, _, _, _)| *name == fixture.name)
            .unwrap_or_else(|| panic!("unmapped ethogram fixture {:?}", fixture.name));
        let expected = fs::read_to_string(Path::new(FIXTURES).join(case).join(source_file))
            .expect("read corresponding umwelt fixture");
        let ours = parse_event(
            expected
                .lines()
                .nth(line_number - 1)
                .expect("corresponding umwelt event line"),
        )
        .expect("parse corresponding umwelt event");
        let upstream = fixture.parse().expect("parse ethogram fixture");

        validate(&ours.event_type, &ours.payload).expect("validate umwelt fixture");
        validate(&upstream.event_type, &upstream.payload).expect("validate ethogram fixture");
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
        compared += 1;
    }
    assert!(compared > 0, "the corpus cross-check must compare fixtures");
    assert_eq!(compared, CORRESPONDING_EVENTS.len());
}

/// The directory holding umwelt's crates: the parent of this crate's own
/// manifest directory, so it follows the tree wherever it is checked out
/// rather than counting levels up to a workspace root that may not be ours.
fn umwelt_crates_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("umwelt-capture sits inside a crates directory")
        .to_path_buf()
}

/// Every `.rs` file under `umwelt-*/src`, recursively. Crates are discovered
/// rather than listed, so a crate added later is covered without editing this;
/// the `umwelt-` filter is what keeps the scan on our own code.
///
/// Both halves matter. Without discovery this stops covering a new crate
/// silently. Without the filter it is one directory layout away from scanning
/// a host workspace's crates — and umwelt is about to be folded into ostrom,
/// which emits `run.started` in nine files quite legitimately, so an unfiltered
/// scan would fail this test on someone else's correct code.
///
/// Test and build-script sources are out of scope on purpose: the premise
/// being checked is about what umwelt emits, and a test naming the string is
/// not umwelt emitting it.
fn rust_source_files(crates_dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut scanned = 0;
    for entry in fs::read_dir(crates_dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", crates_dir.display()))
    {
        let crate_dir = entry.expect("crate dir entry").path();
        if !crate_dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("umwelt-"))
        {
            continue;
        }
        let src = crate_dir.join("src");
        if src.is_dir() {
            collect_rust_files(&src, &mut files);
            scanned += 1;
        }
    }
    // A scan that reaches nothing passes every assertion made over it. This
    // floor is what stops the guard going quietly vacuous if the layout moves.
    assert!(
        scanned >= 2,
        "expected to scan every umwelt crate's src tree, scanned {scanned}"
    );
    files
}

fn collect_rust_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
    {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}

fn initialised_normaliser() -> ClaudeNormaliser {
    let mut normaliser = ClaudeNormaliser::new();
    normaliser
        .line(
            &json!({
                "type": "system",
                "subtype": "init",
                "session_id": "result-session",
                "model": "claude-test"
            })
            .to_string(),
        )
        .expect("normalise init");
    normaliser
}

fn assert_non_success_result(subtype: &str, warning: &str) {
    let mut normaliser = initialised_normaliser();
    let drafts = normaliser
        .line(
            &json!({
                "type": "result",
                "subtype": subtype,
                "session_id": "result-session",
                "total_cost_usd": 0.0223935,
                "usage": {
                    "input_tokens": 9,
                    "output_tokens": 193,
                    "cache_read_input_tokens": 13615,
                    "cache_creation_input_tokens": 9517
                },
                "num_turns": 2,
                "duration_ms": 3380
            })
            .to_string(),
        )
        .expect("non-success result with totals must map");

    assert_eq!(drafts.len(), 2);
    assert_eq!(drafts[0].event_type, AGENT_COMPLETED);
    assert_eq!(drafts[0].payload["costUsd"], json!(0.0223935));
    assert_eq!(drafts[0].payload["turns"], 2);
    assert_eq!(drafts[0].payload["durationMs"], 3380);
    assert_eq!(drafts[0].payload["sessionId"], "result-session");
    assert_eq!(
        drafts[0].payload["usage"],
        json!({
            "inputTokens": 9,
            "outputTokens": 193,
            "cacheReadTokens": 13615,
            "cacheCreationTokens": 9517
        })
    );
    assert_eq!(drafts[1].event_type, AGENT_WARNING);
    assert_eq!(drafts[1].payload["message"], warning);
}

fn normalise_fixture(case: &str) -> Vec<ethogram::EventDraft> {
    let raw = fs::read_to_string(Path::new(FIXTURES).join(case).join("raw.ndjson"))
        .unwrap_or_else(|error| panic!("read {case} capture: {error}"));
    let mut normaliser = ClaudeNormaliser::new();
    let mut drafts = Vec::new();
    for line in raw.lines() {
        drafts.extend(
            normaliser
                .line(line)
                .unwrap_or_else(|error| panic!("normalise {case} frame: {error}")),
        );
    }
    drafts.extend(
        normaliser
            .finish()
            .unwrap_or_else(|error| panic!("finish {case} capture: {error}")),
    );
    drafts
}

fn tool_result<'a>(
    drafts: &'a [ethogram::EventDraft],
    tool_use_id: &str,
) -> &'a ethogram::EventDraft {
    drafts
        .iter()
        .find(|draft| {
            draft.event_type == AGENT_TOOL_RESULT
                && draft.payload["toolUseId"].as_str() == Some(tool_use_id)
        })
        .unwrap_or_else(|| panic!("missing agent.tool_result for {tool_use_id}"))
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
