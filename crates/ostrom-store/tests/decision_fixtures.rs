#![cfg(unix)]

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use ethogram::{DecisionAnsweredPayload, DecisionRequestedPayload, Event};
use ostrom_store::{
    Clock, GateOptions, OstromPaths, PassError, PassRequest, PassRole, QueueDecision, TraceAppend,
    answer_queue_decision, append_trace, run_gate, run_pass,
};
use serde_json::{Map, json};
use umwelt_runtime::{FileSink, Source};

const SUBJECT: &str = "fixture-org/decisions#7";
const CAPTURE_TIME: &str = "2030-01-02T03:04:05.000Z";

fn clock() -> Clock {
    Clock::fixed(CAPTURE_TIME.parse().expect("fixture time"))
}

fn paths() -> OstromPaths {
    // Relative paths keep the account subject and generated budget prose stable
    // without rewriting any narration after capture.
    OstromPaths {
        config: PathBuf::from("fixture-org/decisions"),
        state: PathBuf::from("fixture-org/decisions"),
    }
}

fn executable(path: &Path, script: &str) {
    fs::write(path, script).expect("write fixture executable");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("fixture executable mode");
}

fn in_fixture_process(test: &str) -> bool {
    if std::env::var("OSTROM_DECISION_FIXTURE_CHILD").as_deref() == Ok(test) {
        return true;
    }
    // Isolate cwd and environment in a child, rather than mutating process-wide
    // state shared with other tests. Only fixture forge responses are reachable.
    let root = tempfile::tempdir().expect("decision capture directory");
    let bin = root.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(root.path().join(paths().state)).unwrap();
    fs::write(
        root.path().join("gate-roster.json"),
        include_str!("fixtures/decision-events/gate-roster.json"),
    )
    .unwrap();
    executable(
        &bin.join("gh"),
        r#"#!/usr/bin/env python3
import json, pathlib, sys
roster = json.loads(pathlib.Path('gate-roster.json').read_text())
args = sys.argv[1:]
if args == ['api', 'user']:
    key = 'identity'
elif args[:2] == ['pr', 'view']:
    assert args[2:5] == ['7', '--repo', 'fixture-org/decisions']
    key = 'metadata'
elif args[:2] == ['pr', 'diff']:
    assert '--name-only' in args
    print('src/fixture.rs')
    sys.exit(0)
elif args[:2] == ['api', 'graphql']:
    key = 'threads'
elif args[0] == 'api' and args[1].startswith('repos/fixture-org/decisions/commits/'):
    key = 'checks' if '/check-runs?' in args[1] else 'status'
else:
    sys.exit('unexpected fixture forge call: ' + repr(args))
print(json.dumps(roster[key]))
"#,
    );
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture"])
        .current_dir(root.path())
        .env_clear()
        .env("OSTROM_DECISION_FIXTURE_CHILD", test)
        .env("MANDATE_DAILY_CAP_USD", "50")
        .env(
            "PATH",
            std::env::join_paths([bin, PathBuf::from("/usr/bin"), PathBuf::from("/bin")]).unwrap(),
        );
    if let Some(capture) = std::env::var_os("OSTROM_CAPTURE_DECISION_FIXTURES") {
        command.env("OSTROM_CAPTURE_DECISION_FIXTURES", capture);
    }
    let output = command.output().expect("run isolated decision capture");
    assert!(output.status.success(), "{output:?}");
    false
}

fn read_run(run_id: &str, types: &[&str]) -> Vec<Event> {
    let events = FileSink::new(paths().runs_dir())
        .read_from(run_id, 0)
        .expect("read complete sink stream");
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        types
    );
    for (index, event) in events.iter().enumerate() {
        assert_eq!(
            event.seq,
            index as u64 + 1,
            "true position in the full stream"
        );
        ethogram::validate(&event.event_type, &event.payload).expect("valid emitted event");
    }
    events
}

fn generated_run(prefix: &str) -> String {
    let run_ids = fs::read_dir(paths().runs_dir())
        .unwrap()
        .map(|entry| {
            let stream = fs::read_to_string(entry.unwrap().path().join("events.jsonl")).unwrap();
            ethogram::parse_event(stream.lines().next().unwrap())
                .unwrap()
                .run_id
        })
        .filter(|id| id.starts_with(&format!("{prefix}-")))
        .collect::<Vec<_>>();
    assert_eq!(run_ids.len(), 1);
    run_ids[0].clone()
}

fn normalize_generated_id(id: &str, prefix: &str) -> String {
    let stem = format!("{prefix}-20300102T030405000Z-{}-", std::process::id());
    let sequence = id
        .strip_prefix(&stem)
        .expect("real generated id with fixture clock");
    sequence.parse::<u64>().expect("generated id counter");
    format!("{prefix}-20300102T030405000Z-fixture-{sequence}")
}

fn assert_capture(name: &str, event: &Event) {
    let mut captured = event.clone();
    // FileSink owns wall time; retain its seq unchanged. Only the process ID in
    // generated identifiers is normalized; clock, prefix and counter are kept.
    captured.ts = CAPTURE_TIME.to_owned();
    if captured.run_id != "gate" {
        let prefix = if captured.event_type == ethogram::DECISION_ANSWERED {
            "judgment"
        } else {
            "builder"
        };
        captured.run_id = normalize_generated_id(&captured.run_id, prefix);
    }
    if captured.payload["kind"] == "budget" {
        captured.payload["decisionId"] = json!(normalize_generated_id(
            captured.payload["decisionId"].as_str().unwrap(),
            "budget"
        ));
    }
    let emitted = format!("{}\n", serde_json::to_string_pretty(&captured).unwrap());
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/decision-events/expected")
        .join(name);
    if std::env::var_os("OSTROM_CAPTURE_DECISION_FIXTURES").is_some() && !path.exists() {
        // Capture missing files only. Published fixtures can never be overwritten.
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap()
            .write_all(emitted.as_bytes())
            .unwrap();
    }
    let expected = fs::read_to_string(&path).expect("captured fixture must exist");
    assert_eq!(emitted, expected, "capture {}", path.display());
    let parsed = ethogram::parse_event(expected.trim()).expect("fixture parses at pinned SDK");
    ethogram::validate(&parsed.event_type, &parsed.payload)
        .expect("fixture validates at pinned SDK");
}

#[test]
fn gate_and_answer_decision_fixtures() {
    if !in_fixture_process("gate_and_answer_decision_fixtures") {
        return;
    }
    fs::write(paths().config.join("gate.yaml"), "provider: file\nbounce_all: []\nprojects:\n  - repo: fixture-org/decisions\n    required_checks: [fixture-check]\n    bounce: []\n    reserved: []\n").unwrap();
    let output = run_gate(&GateOptions {
        paths: paths(),
        working_directory: PathBuf::from("."),
        target: SUBJECT.to_owned(),
        timestamp: clock().timestamp(),
    })
    .expect("run gate over fixture roster");
    assert_eq!(output.exit_code, 2, "{output:?}");
    let events = read_run("gate", &["run.started", "decision.requested"]);
    let request: DecisionRequestedPayload =
        serde_json::from_value(events[1].payload.clone()).unwrap();
    assert_eq!(request.kind, ethogram::DecisionKind::GateInconclusive);
    assert_eq!(request.subject.as_deref(), Some(SUBJECT));
    assert_eq!(
        request
            .options
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        ["excuse:mergeable", "excuse:required_checks", "wait", "fail"]
    );
    assert_capture("gate-inconclusive.json", &events[1]);

    answer_queue_decision(
        &paths(),
        SUBJECT,
        QueueDecision::Approve,
        &request.decision_id,
        "excuse:required_checks",
        &clock(),
    )
    .expect("answer the captured gate decision");
    let answers = read_run(
        &generated_run("judgment"),
        &["run.started", "decision.answered", "run.finished"],
    );
    let answer: DecisionAnsweredPayload =
        serde_json::from_value(answers[1].payload.clone()).unwrap();
    assert_eq!(answer.decision_id, request.decision_id);
    assert_eq!(answer.option_id, "excuse:required_checks");
    assert_eq!(answer.reversal.as_deref(), Some("revoke:required_checks"));
    assert_eq!(answer.by, "github:user:42");
    assert_eq!(answers[0].payload["kind"], "judgment");
    assert_eq!(answers[2].payload["outcome"], "completed");
    assert_capture("decision-answered.json", &answers[1]);
}

#[test]
fn budget_decision_fixture() {
    if !in_fixture_process("budget_decision_fixture") {
        return;
    }
    fs::create_dir_all(paths().state.join("roles")).unwrap();
    fs::write(paths().state.join("roles/builder.settings.json"), "{}\n").unwrap();
    fs::write(paths().state.join("loop-armed"), "").unwrap();
    executable(
        Path::new("bin/fixture-harness"),
        "#!/bin/sh\ntouch harness-spawned\nexit 97\n",
    );
    append_trace(
        &paths().trace_file(),
        &TraceAppend {
            ts: clock().timestamp(),
            kind: "pass-ended".to_owned(),
            fact: Map::from_iter([("cost_usd".to_owned(), json!(50))]),
            narration: Map::new(),
        },
    )
    .unwrap();
    let result = run_pass(&PassRequest {
        paths: paths(),
        working_directory: PathBuf::from("."),
        role: PassRole::Builder,
        prompt: "Work on the fixture repository.".to_owned(),
        permission_mode: PassRole::Builder.default_permission_mode(),
        derived_settings: None,
        claude_bin: PathBuf::from("bin/fixture-harness"),
        signals: Default::default(),
        supervisor_pid: None,
        events_fd: None,
        control_fd: None,
        facts_only: false,
        caps: Default::default(),
        clock: clock(),
        platform: std::env::consts::OS,
    });
    assert!(
        matches!(result, Err(PassError::BudgetHeld(_))),
        "{result:?}"
    );
    assert!(!Path::new("harness-spawned").exists());
    let events = read_run(
        &generated_run("builder"),
        &["run.started", "decision.requested", "run.finished"],
    );
    let request: DecisionRequestedPayload =
        serde_json::from_value(events[1].payload.clone()).unwrap();
    assert_eq!(request.kind, ethogram::DecisionKind::Budget);
    assert_eq!(
        request
            .options
            .iter()
            .map(|option| option.id.as_str())
            .collect::<Vec<_>>(),
        ["raise", "wait"]
    );
    assert_eq!(events[2].payload["outcome"], "blocked");
    assert_eq!(events[2].payload["reason"], "budget");
    assert_capture("budget.json", &events[1]);
}
