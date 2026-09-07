#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};

use ethogram::{DecisionRequestedPayload, EventDraft, RunKind, RunOutcome};
use ostrom_core::sha256_hex;
use ostrom_store::{
    Clock, OstromPaths, QueueDecision, QueueDocument, RunEventGuard, RunEventStart,
    decide_queue_item, write_queue,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use umwelt_runtime::{FileSink, Sink, Source};

const SUBJECT: &str = "example-org/example-repo#19";
const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Fixture {
    root: TempDir,
    paths: OstromPaths,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let paths = OstromPaths {
            config: root.path().join("state"),
            state: root.path().join("state"),
        };
        let bin = root.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&paths.state).unwrap();
        let script = bin.join("gh");
        fs::write(&script, r#"#!/usr/bin/env python3
import json, os, pathlib, sys
root = pathlib.Path(os.environ['OSTROM_HOME'])
args = sys.argv[1:]
with (root / 'gh-calls.jsonl').open('a') as f: f.write(json.dumps(args) + '\n')
if args == ['api', 'user']:
    print((root / 'identity.json').read_text() if (root / 'identity.json').exists() else '{"id":42,"login":"fixture-principal"}')
elif args[:2] == ['api', 'repos/example-org/example-repo/issues/19']:
    if '--method' in args:
        facts = [json.loads(row) for row in (root / 'sprint.jsonl').read_text().splitlines()]
        assert any(row['kind'] == 'decision-answered' for row in facts)
        events = [json.loads(row) for path in (root / 'runs').glob('*/events.jsonl') for row in path.read_text().splitlines()]
        assert any(row['type'] == 'decision.answered' for row in events)
        if (root / 'fail-tick').exists():
            print('HTTP 403: body edit denied', file=sys.stderr)
            sys.exit(1)
        body = json.load(sys.stdin)['body']
        (root / 'written-body.txt').write_text(body)
        print(json.dumps({'body': body}))
    else:
        if (root / 'body-response.json').exists():
            print((root / 'body-response.json').read_text())
            sys.exit(0)
        count_path = root / 'read-count'
        count = int(count_path.read_text()) if count_path.exists() else 0
        count_path.write_text(str(count + 1))
        path = root / ('later-body.txt' if count > 0 and (root / 'later-body.txt').exists() else 'body.txt')
        print(json.dumps({'body': path.read_bytes().decode()}))
elif args[:2] == ['pr', 'view']:
    print('{"headRefOid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}')
else:
    print('unexpected gh call: ' + repr(args), file=sys.stderr)
    sys.exit(97)
"#).unwrap();
        fs::set_permissions(script, fs::Permissions::from_mode(0o755)).unwrap();
        Self { root, paths, bin }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ostrom"))
            .args(args)
            .env_clear()
            .env("HOME", self.root.path())
            .env(
                "PATH",
                std::env::join_paths([
                    self.bin.clone(),
                    PathBuf::from("/usr/bin"),
                    PathBuf::from("/bin"),
                ])
                .unwrap(),
            )
            .env("OSTROM_HOME", &self.paths.state)
            .current_dir(self.root.path())
            .output()
            .unwrap()
    }

    fn answer(&self, verb: &str, id: &str, option: &str) -> Output {
        self.run(&["queue", verb, SUBJECT, "--decision", id, "--option", option])
    }

    fn queue(&self) {
        let row = QueueDocument::from_value(json!({
            "id":SUBJECT, "repo":"example-org/example-repo", "ref":"#19", "kind":"tripwire",
            "mandate":{"reason":"fixture tripwire", "dossier":{"question":"private dossier", "options_ruled_out":[], "recommended_action":"review", "blast_radius":"one item"}},
            "state":"pending", "opened":"2026-09-01T00:00:00Z", "needs_judgment":true
        })).unwrap();
        write_queue(&self.paths.queue_file(), &[row]).unwrap();
    }

    fn request(&self, id: &str, kind: &str, options: &[&str]) -> DecisionRequestedPayload {
        self.request_on("request-run", id, kind, options)
    }

    fn request_on(
        &self,
        run_id: &str,
        id: &str,
        kind: &str,
        options: &[&str],
    ) -> DecisionRequestedPayload {
        let mut run = RunEventGuard::start(
            &self.paths,
            None,
            false,
            Clock::realtime(),
            RunEventStart {
                run_id: run_id.to_owned(),
                kind: RunKind::Loop,
                actor: "fixture-role".to_owned(),
                harness: "ostrom".to_owned(),
                model: None,
                schedule: None,
                repository: None,
                work_order: None,
                ceilings: None,
            },
        )
        .unwrap();
        let payload = json!({"decisionId":id, "kind":kind,
            "dossier":{"question":"private dossier", "optionsRuledOut":[], "recommendedAction":"review", "blastRadius":"one item"},
            "options":options.iter().map(|id| json!({"id":id,"label":id})).collect::<Vec<_>>(), "subject":SUBJECT});
        FileSink::new(self.paths.runs_dir())
            .append(
                run_id,
                EventDraft {
                    event_type: ethogram::DECISION_REQUESTED.to_owned(),
                    payload: payload.clone(),
                    captured_at: None,
                },
            )
            .unwrap();
        run.finish(RunOutcome::Completed, None, None, None).unwrap();
        serde_json::from_value(payload).unwrap()
    }

    fn gate_request(&self, head: &str) -> String {
        let digest = "sha256:fixture-gate-digest";
        let id = format!(
            "gate_inconclusive-{}",
            sha256_hex(format!("gate_inconclusive\0{SUBJECT}\0{head}\0{digest}").as_bytes())
        );
        fs::write(
            self.paths.state.join("gate.jsonl"),
            format!(
                "{}\n",
                json!({"pr":SUBJECT,"head_sha":head,"judgment_digest":digest})
            ),
        )
        .unwrap();
        self.request(
            &id,
            "gate_inconclusive",
            &["excuse:required_checks", "wait", "fail"],
        );
        id
    }

    fn human_request(&self, body: &str, question: &str) -> String {
        fs::write(self.paths.state.join("body.txt"), body).unwrap();
        let id = format!(
            "human-decides-{}",
            sha256_hex(format!("human_decides\0{SUBJECT}\0{question}").as_bytes())
        );
        self.request(&id, "human_decides", &["yes", "no"]);
        id
    }

    fn rows(&self, file: &str) -> Vec<Value> {
        fs::read_to_string(self.paths.state.join(file))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn events(&self) -> Vec<ethogram::Event> {
        fs::read_dir(self.paths.runs_dir())
            .unwrap()
            .flat_map(|entry| {
                fs::read_to_string(entry.unwrap().path().join("events.jsonl"))
                    .unwrap()
                    .lines()
                    .map(|line| ethogram::parse_event(line).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn answered(&self) -> Vec<Value> {
        self.rows("sprint.jsonl")
            .into_iter()
            .filter(|row| row["kind"] == "decision-answered")
            .collect()
    }
}

fn success(output: &Output) {
    assert!(output.status.success(), "{output:?}");
}
fn refused(output: &Output, message: &str) {
    assert!(!output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(message),
        "{output:?}"
    );
}

#[test]
fn no_flags_preserve_queue_approve_bytes_and_create_no_run_or_answer() {
    let cli = Fixture::new();
    let direct = Fixture::new();
    cli.queue();
    direct.queue();
    let expected = decide_queue_item(
        &direct.paths.queue_file(),
        &direct.paths.sweep_state_file(),
        &direct.paths.selector_events_file(),
        SUBJECT,
        QueueDecision::Approve,
        None,
    )
    .unwrap();
    let output = cli.run(&["queue", "approve", SUBJECT]);
    success(&output);
    assert_eq!(output.stdout, expected);
    assert_eq!(
        fs::read(cli.paths.queue_file()).unwrap(),
        fs::read(direct.paths.queue_file()).unwrap()
    );
    assert!(!cli.paths.runs_dir().exists());
    assert!(cli.answered().is_empty());
    assert!(cli.rows("gh-calls.jsonl").is_empty());
}

#[test]
fn answers_use_a_new_judgment_run_after_the_request_run_finished() {
    for (verb, reversal) in [
        ("approve", "reject"),
        ("reject", "approve"),
        ("defer", "approve"),
    ] {
        let fixture = Fixture::new();
        fixture.queue();
        let request = fixture.request("decision-1", "tripwire", &["approve", "reject", "defer"]);
        let before = FileSink::new(fixture.paths.runs_dir())
            .read_from("request-run", 0)
            .unwrap();
        success(&fixture.answer(verb, "decision-1", verb));
        assert_eq!(
            FileSink::new(fixture.paths.runs_dir())
                .read_from("request-run", 0)
                .unwrap(),
            before
        );
        let events = fixture.events();
        let event = events
            .iter()
            .find(|event| event.event_type == ethogram::DECISION_ANSWERED)
            .unwrap();
        let answer: ethogram::DecisionAnsweredPayload =
            serde_json::from_value(event.payload.clone()).unwrap();
        ethogram::validate_decision_answer_against_request(&request, &answer).unwrap();
        assert_eq!(answer.option_id, verb);
        assert_eq!(answer.by, "github:user:42");
        assert_eq!(answer.reversal.as_deref(), Some(reversal));
        assert!(event.payload.get("requestedRunId").is_none());
        assert!(event.payload.get("byTimeout").is_none());
        let run = FileSink::new(fixture.paths.runs_dir())
            .read_from(&event.run_id, 0)
            .unwrap();
        assert_eq!(
            run.iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            ["run.started", "decision.answered", "run.finished"]
        );
        assert_eq!(run[0].payload["kind"], "judgment");
        assert_eq!(run[2].payload["outcome"], "completed");
        let facts = fixture.answered();
        assert_eq!(facts.len(), 1);
        assert_eq!(
            facts[0]["fact"],
            json!({"decision_id":"decision-1","option":verb,"by":"github:user:42","reversal":reversal})
        );
        ostrom_core::EventPayload::new(facts[0]["fact"].as_object().unwrap().clone()).unwrap();
        assert!(
            !fs::read_to_string(fixture.paths.trace_file())
                .unwrap()
                .contains("dossier")
        );
        // Opposite verbs remain applicable after approve and after reject removed the row.
        success(&fixture.answer(reversal, "decision-1", reversal));
        assert_eq!(fixture.answered().len(), 2);
    }
}

#[test]
fn an_unoffered_option_trips_ethograms_helper_before_any_delivery_or_record() {
    let fixture = Fixture::new();
    fixture.queue();
    fixture.request("decision-1", "tripwire", &["approve", "reject", "defer"]);
    let before = fs::read(fixture.paths.queue_file()).unwrap();
    refused(
        &fixture.answer("approve", "decision-1", "invented"),
        "optionId does not name a request option",
    );
    assert_eq!(fs::read(fixture.paths.queue_file()).unwrap(), before);
    assert!(fixture.answered().is_empty());
    assert_eq!(fixture.events().len(), 3);
    assert!(fixture.rows("gh-calls.jsonl").is_empty());
}

#[test]
fn paired_flags_subject_verb_identity_and_duplicate_guards_refuse() {
    let fixture = Fixture::new();
    fixture.queue();
    fixture.request("decision-1", "tripwire", &["approve", "reject", "defer"]);
    for args in [
        vec!["queue", "approve", SUBJECT, "--decision", "decision-1"],
        vec!["queue", "approve", SUBJECT, "--option", "approve"],
    ] {
        refused(&fixture.run(&args), "required");
    }
    refused(
        &fixture.answer("approve", "missing", "approve"),
        "unknown decision",
    );
    refused(
        &fixture.run(&[
            "queue",
            "approve",
            "example-org/example-repo#20",
            "--decision",
            "decision-1",
            "--option",
            "approve",
        ]),
        "subject does not match",
    );
    refused(
        &fixture.answer("approve", "decision-1", "reject"),
        "verb must agree",
    );
    fs::write(fixture.paths.state.join("identity.json"), "{}").unwrap();
    refused(
        &fixture.answer("approve", "decision-1", "approve"),
        "no principal identity",
    );
    assert!(fixture.answered().is_empty());
    fs::remove_file(fixture.paths.state.join("identity.json")).unwrap();
    success(&fixture.answer("approve", "decision-1", "approve"));
    refused(
        &fixture.answer("approve", "decision-1", "approve"),
        "already recorded",
    );
    assert_eq!(fixture.answered().len(), 1);
}

#[test]
fn raise_and_wait_record_reversals_without_changing_any_policy() {
    let fixture = Fixture::new();
    fixture.request("budget-1", "budget", &["raise", "wait"]);
    let manifest = "operator policy must remain byte identical\n";
    fs::write(fixture.paths.state.join("ostrom.yaml"), manifest).unwrap();
    for (option, reversal) in [("raise", "wait"), ("wait", "raise")] {
        success(&fixture.answer("approve", "budget-1", option));
        assert_eq!(
            fixture.answered().last().unwrap()["fact"]["reversal"],
            reversal
        );
    }
    assert_eq!(
        fs::read_to_string(fixture.paths.state.join("ostrom.yaml")).unwrap(),
        manifest
    );
    assert!(!fixture.paths.queue_file().exists());
    assert!(!fixture.paths.state.join("exceptions.jsonl").exists());
}

#[test]
fn excuse_and_its_recorded_reversal_use_the_recorded_sha() {
    let fixture = Fixture::new();
    let id = fixture.gate_request(HEAD);
    refused(
        &fixture.answer("approve", &id, "revoke:required_checks"),
        "recorded excuse reversal",
    );
    refused(
        &fixture.answer("approve", &id, "excuse:unoffered"),
        "optionId does not name a request option",
    );
    success(&fixture.answer("approve", &id, "excuse:required_checks"));
    assert_eq!(
        fixture.answered()[0]["fact"]["reversal"],
        "revoke:required_checks"
    );
    let events = fixture.events();
    let grant = events
        .iter()
        .find(|event| event.event_type == ethogram::DECISION_ANSWERED)
        .unwrap();
    ethogram::validate(&grant.event_type, &grant.payload).unwrap();
    assert_eq!(grant.payload["reversal"], "revoke:required_checks");
    // ethogram#7's ruled reversal is outside the offered options at ba892e84;
    // switch to ethogram#35's relaxed helper at a later independent repin.
    success(&fixture.answer("approve", &id, "revoke:required_checks"));
    let exceptions = fixture.rows("exceptions.jsonl");
    assert_eq!(exceptions.len(), 2);
    assert!(exceptions.iter().all(|row| row["head_sha"] == HEAD));
    assert!(exceptions[0].get("revoked").is_none());
    assert_eq!(exceptions[1]["revoked"], true);
    assert_eq!(
        fixture.answered()[1]["fact"]["reversal"],
        "excuse:required_checks"
    );
    assert!(
        fixture
            .rows("gh-calls.jsonl")
            .iter()
            .all(|args| args[0] != "pr")
    );
}

#[test]
fn missing_or_invalid_recorded_heads_cannot_grant() {
    for head in ["", "not-a-sha"] {
        let fixture = Fixture::new();
        let id = fixture.gate_request(head);
        refused(
            &fixture.answer("approve", &id, "excuse:required_checks"),
            "no recorded full head SHA",
        );
        assert!(fixture.answered().is_empty());
        assert!(fixture.rows("exceptions.jsonl").is_empty());
    }
    let fixture = Fixture::new();
    let id = fixture.gate_request(HEAD);
    fs::write(fixture.paths.state.join("gate.jsonl"), "").unwrap();
    refused(
        &fixture.answer("approve", &id, "excuse:required_checks"),
        "no gate record matches",
    );
    assert!(fixture.answered().is_empty());
}

#[test]
fn checkbox_delivery_changes_exactly_one_byte_in_one_row() {
    let fixture = Fixture::new();
    let body = "Opening  text\r\n### Human decides\r\n- [ ] Another row\r\n- [ ] **May  it ship?**\r\n  With this  condition.\r\n  - [ ] yes\r\n  - [ ] no\r\n- [x] Done\r\n\r\n### AI implements\r\n- [ ] **May  it ship?**\r\n";
    let id = fixture.human_request(body, "**May it ship?** With this condition.");
    success(&fixture.answer("approve", &id, "yes"));
    let written = fs::read(fixture.paths.state.join("written-body.txt")).unwrap();
    let expected = body.replacen("- [ ] **May  it ship?**", "- [x] **May  it ship?**", 1);
    assert_eq!(written, expected.as_bytes());
    assert_eq!(written.len(), body.len());
    assert_eq!(
        written
            .iter()
            .zip(body.bytes())
            .filter(|(left, right)| **left != *right)
            .count(),
        1
    );
    assert_eq!(fixture.answered()[0]["fact"]["reversal"], "no");
    assert_eq!(
        fs::read_to_string(fixture.paths.state.join("read-count")).unwrap(),
        "2"
    );
}

#[test]
fn concurrent_body_change_aborts_the_tick_but_preserves_the_answer() {
    let fixture = Fixture::new();
    let body = "### Human decides\n- [ ] Ship?\n";
    let id = fixture.human_request(body, "Ship?");
    let changed = format!("{body}Someone else's new paragraph\n");
    fs::write(fixture.paths.state.join("later-body.txt"), &changed).unwrap();
    refused(
        &fixture.answer("approve", &id, "yes"),
        "body changed before delivery",
    );
    assert!(!fixture.paths.state.join("written-body.txt").exists());
    assert_eq!(
        fs::read_to_string(fixture.paths.state.join("later-body.txt")).unwrap(),
        changed
    );
    assert_eq!(fixture.answered().len(), 1);
    assert!(
        fixture
            .events()
            .iter()
            .any(|event| event.event_type == ethogram::DECISION_ANSWERED)
    );
}

#[test]
fn already_ticked_missing_and_ambiguous_rows_refuse_without_a_write() {
    for body in [
        "### Human decides\n- [x] Ship?\n",
        "### Human decides\n- [ ] Different row\n",
        "### Human decides\n- [ ] Ship?\n- [ ] Ship?\n",
        "```\n### Human decides\n- [ ] Ship?\n```\n",
    ] {
        let fixture = Fixture::new();
        let id = fixture.human_request(body, "Ship?");
        refused(
            &fixture.answer("approve", &id, "yes"),
            "exactly one matching unticked",
        );
        assert!(!fixture.paths.state.join("written-body.txt").exists());
        assert_eq!(fixture.answered().len(), 1);
    }
}

#[test]
fn failed_patch_keeps_the_answer_and_a_retry_does_not_duplicate_or_redeliver() {
    let fixture = Fixture::new();
    let id = fixture.human_request("### Human decides\n- [ ] Ship?\n", "Ship?");
    fs::write(fixture.paths.state.join("fail-tick"), "").unwrap();
    let output = fixture.answer("approve", &id, "yes");
    refused(
        &output,
        "answer recorded; could not tick example-org/example-repo#19",
    );
    refused(&output, "body edit denied");
    assert_eq!(fixture.answered().len(), 1);
    let events = fixture.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == ethogram::DECISION_ANSWERED)
            .count(),
        1
    );
    let calls = fixture.rows("gh-calls.jsonl");
    refused(&fixture.answer("approve", &id, "yes"), "already recorded");
    assert_eq!(fixture.rows("gh-calls.jsonl"), calls);
    assert_eq!(fixture.answered().len(), 1);
}

#[test]
fn an_inflight_answer_and_conflicting_requests_refuse() {
    let fixture = Fixture::new();
    fixture.request("decision-1", "tripwire", &["approve", "reject", "defer"]);
    fs::write(fixture.paths.state.join("decision-answer.lock"), "").unwrap();
    refused(
        &fixture.answer("approve", "decision-1", "approve"),
        "cannot lock answers",
    );
    fs::remove_file(fixture.paths.state.join("decision-answer.lock")).unwrap();
    fixture.request_on("other-run", "decision-1", "budget", &["raise", "wait"]);
    refused(
        &fixture.answer("approve", "decision-1", "approve"),
        "conflicting requests",
    );
    assert!(fixture.answered().is_empty());
}

#[test]
fn standalone_excuse_revoke_accepts_an_explicit_head_and_validates_it() {
    let fixture = Fixture::new();
    success(&fixture.run(&[
        "excuse",
        "grant",
        SUBJECT,
        "required_checks",
        "--head-sha",
        HEAD,
        "fixture reason",
    ]));
    success(&fixture.run(&[
        "excuse",
        "revoke",
        SUBJECT,
        "required_checks",
        "--head-sha",
        HEAD,
        "correction",
    ]));
    assert_eq!(fixture.rows("exceptions.jsonl")[1]["revoked"], true);
    refused(
        &fixture.run(&[
            "excuse",
            "revoke",
            SUBJECT,
            "required_checks",
            "--head-sha",
            "invalid",
            "correction",
        ]),
        "full 40-character head SHA",
    );
    assert_eq!(fixture.rows("exceptions.jsonl").len(), 2);
}

#[test]
fn wait_fail_and_custom_answers_carry_corrective_options() {
    for (option, reversal) in [("wait", "fail"), ("fail", "wait")] {
        let fixture = Fixture::new();
        fixture.request("gate-1", "gate_inconclusive", &["wait", "fail"]);
        success(&fixture.answer("approve", "gate-1", option));
        assert_eq!(fixture.answered()[0]["fact"]["reversal"], reversal);
    }
    let fixture = Fixture::new();
    let body =
        "### Human decides\n- [ ] Choose a direction\n  - [ ] East\n  - [ ] West\n  - [ ] North\n";
    fs::write(fixture.paths.state.join("body.txt"), body).unwrap();
    let id = format!(
        "human-decides-{}",
        sha256_hex(format!("human_decides\0{SUBJECT}\0Choose a direction").as_bytes())
    );
    fixture.request(&id, "human_decides", &["east", "west", "north"]);
    for (option, reversal) in [("east", "west"), ("north", "east")] {
        success(&fixture.answer("approve", &id, option));
        assert_eq!(
            fixture.answered().last().unwrap()["fact"]["reversal"],
            reversal
        );
    }
}

#[test]
fn unsupported_deliveries_and_answers_without_a_reversal_refuse() {
    for (kind, options, option, message) in [
        (
            "permission",
            vec!["allow", "deny"],
            "allow",
            "no ostrom delivery or reversal",
        ),
        (
            "budget",
            vec!["spend", "wait"],
            "spend",
            "no ostrom delivery or reversal",
        ),
        (
            "human_decides",
            vec!["only"],
            "only",
            "no corrective alternative",
        ),
    ] {
        let fixture = Fixture::new();
        fixture.request("decision-1", kind, &options);
        refused(&fixture.answer("approve", "decision-1", option), message);
        assert!(fixture.answered().is_empty());
        assert!(fixture.rows("gh-calls.jsonl").is_empty());
    }
}

#[test]
fn a_malformed_answer_ledger_is_not_ignored() {
    let fixture = Fixture::new();
    fixture.request("budget-1", "budget", &["raise", "wait"]);
    fs::write(fixture.paths.trace_file(), "not a trace row\n").unwrap();
    refused(
        &fixture.answer("approve", "budget-1", "raise"),
        "malformed sprint trace",
    );
    assert_eq!(
        fs::read_to_string(fixture.paths.trace_file()).unwrap(),
        "not a trace row\n"
    );
    assert!(fixture.rows("gh-calls.jsonl").is_empty());
}

#[test]
fn a_forge_response_without_a_body_preserves_the_answer_and_refuses_the_tick() {
    for response in ["{}", "{\"body\":null}"] {
        let fixture = Fixture::new();
        let id = fixture.human_request("### Human decides\n- [ ] Ship?\n", "Ship?");
        fs::write(fixture.paths.state.join("body-response.json"), response).unwrap();
        refused(
            &fixture.answer("approve", &id, "yes"),
            "forge returned no issue body",
        );
        assert_eq!(fixture.answered().len(), 1);
        assert!(!fixture.paths.state.join("written-body.txt").exists());
        let events = fixture.events();
        let answer = events
            .iter()
            .find(|event| event.event_type == ethogram::DECISION_ANSWERED)
            .unwrap();
        let run = FileSink::new(fixture.paths.runs_dir())
            .read_from(&answer.run_id, 0)
            .unwrap();
        assert_eq!(run.last().unwrap().payload["outcome"], "failed");
    }
}

#[test]
fn the_shared_excuse_path_accepts_the_gates_draft_and_mergeable_conditions() {
    let fixture = Fixture::new();
    for condition in ["draft", "mergeable"] {
        success(&fixture.run(&[
            "excuse",
            "grant",
            SUBJECT,
            condition,
            "--head-sha",
            HEAD,
            "explicit correction",
        ]));
        assert_eq!(
            fixture.rows("exceptions.jsonl").last().unwrap()["condition"],
            condition
        );
    }
}
