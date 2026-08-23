use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use chrono::{DateTime, Utc};
use ostrom_checks::ActionRegistry;
use ostrom_core::{
    CHECK_STORE_SCHEMA_VERSION, Catalogue, CatalogueEnumeration, CheckDocument, CheckRun,
    CheckRunId, CheckVerdict, RunnerStamp,
};
use serde_json::{Value, json};
use tempfile::tempdir;

mod support;

const ROSTER: &str = r#"
provider: file
cadence_hours: 1
stuck_after_days: 7
search_roots: []
hold_labels: []
bounce_all: []
projects:
  - repo: example-org/example-repo
    delegated: []
    excluded: []
    reserved: [10]
    default: delegated
    paused: false
    bounce: []
  - repo: another-example-org/another-example-repo
    delegated: [type:fix]
    excluded: []
    reserved: []
    default: excluded
    paused: false
    bounce: []
"#;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sweep-cross-org.json")
}

fn configure(home: &Path) {
    fs::write(home.join("ostrom.yaml"), "manifest_version: 1\n").expect("write repository policy");
    support::sign_manifest(&home.join("ostrom.yaml"));
    fs::write(home.join("mandates.yaml"), ROSTER).expect("write mandates");
    fs::write(
        home.join("gate.jsonl"),
        concat!(
            r#"{"ts":"2026-07-10T00:00:00Z","pr":"example-org/example-repo#1","head_sha":"0000000000000000000000000000000000000000","verdict":"pass"}"#,
            "\n",
        ),
    )
    .expect("write gate evidence");
}

#[cfg(unix)]
fn configure_assessor_case(home: &Path) -> PathBuf {
    configure(home);
    fs::write(
        home.join("goals.yaml"),
        r#"
goals_version: 1
goals:
  - id: rust-cli
    intent: ostrom runs as a product
    state: active
    serves: [{epic: example-org/example-repo#115}]
    met_when: []
actions: []
acknowledgements: []
"#,
    )
    .expect("write goals");
    fs::write(
        home.join("queue.jsonl"),
        concat!(
            r##"{"id":"example-org/example-repo#10","repo":"example-org/example-repo","ref":"#10","title":"Routine maintenance","kind":"decision","mandate":{"reason":"reserved ref:#10"},"state":"approved","opened":"2026-07-01T00:00:00Z","age_days":31,"aged_out":true,"needs_judgment":true,"blocked_by":[]}"##,
            "\n",
        ),
    )
    .expect("write approved queue state");
    let mut fixture_value: Value =
        serde_json::from_slice(&fs::read(fixture()).expect("read fixture")).expect("fixture JSON");
    fixture_value["repositories"][0]["issues"][0]["epic"] = json!("example-org/example-repo#115");
    let plan_fixture = home.join("plan-fixture.json");
    fs::write(
        &plan_fixture,
        serde_json::to_vec(&fixture_value).expect("serialize fixture"),
    )
    .expect("write plan fixture");
    plan_fixture
}

#[cfg(unix)]
fn write_harness_stub(home: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let stub = home.join("placeholder-assessor");
    fs::write(
        &stub,
        concat!(
            "#!/usr/bin/env bash\n",
            "set -eu\n",
            "cat >\"$OSTROM_STUB_INPUT\"\n",
            "printf '%s\\0' \"$@\" >\"$OSTROM_STUB_ARGS\"\n",
            "printf '%s\\n' \"$OSTROM_STUB_OUTPUT\"\n",
        ),
    )
    .expect("write assessor stub");
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o700)).expect("assessor stub mode");
    stub
}

#[cfg(unix)]
fn run_named_assessor(
    home: &Path,
    harness: &str,
    executable: &Path,
    stub_output: &str,
    default_claude: bool,
) -> (Value, Vec<String>, String) {
    let plan_fixture = configure_assessor_case(home);
    let args_file = home.join("assessor-args");
    let input_file = home.join("assessor-input");
    let variable = match harness {
        "claude" => "CLAUDE_BIN",
        "codex" => "CODEX_BIN",
        "copilot" => "COPILOT_BIN",
        _ => panic!("unknown fixture harness"),
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
    command.arg("plan");
    if default_claude {
        command.arg("--assessor");
    } else {
        command.args(["--assessor", harness]);
    }
    let output = command
        .args([
            "--fixture",
            plan_fixture.to_str().expect("fixture path"),
            "--started-at",
            "2026-08-01T00:00:00Z",
        ])
        .env("OSTROM_HOME", home)
        .env(
            "OSTROM_POLICY_TRUSTED_KEYS",
            home.join("trusted-policy-keys"),
        )
        .env_remove("OSTROM_PLAN_DERIVER")
        .env(variable, executable)
        .env("OSTROM_STUB_ARGS", &args_file)
        .env("OSTROM_STUB_INPUT", &input_file)
        .env("OSTROM_STUB_OUTPUT", stub_output)
        .current_dir(home)
        .output()
        .expect("run named assessor");
    assert!(
        output.status.success(),
        "plan stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document = serde_json::from_slice(&fs::read(home.join("plan.json")).expect("plan output"))
        .expect("parse plan");
    let arguments = fs::read(&args_file)
        .expect("stub arguments")
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(|argument| String::from_utf8(argument.to_vec()).expect("UTF-8 stub argument"))
        .collect();
    let prompt = fs::read_to_string(input_file).expect("stub input");
    (document, arguments, prompt)
}

#[cfg(unix)]
fn assert_harness_contract(
    document: &Value,
    arguments: &[String],
    prompt: &str,
    expected_bound: &str,
) {
    assert_eq!(
        document["ranking"]["computed"],
        json!(["example-org/example-repo#10"])
    );
    assert!(document["faults"].as_array().is_some_and(Vec::is_empty));
    let rendered = arguments.join("\n");
    assert!(rendered.contains(expected_bound), "arguments: {rendered}");
    assert!(prompt.starts_with("Assess exactly one authored goal"));
    assert!(prompt.contains(r#""goal":"rust-cli""#));
    assert!(prompt.contains(r#""facts":"#));
    assert!(!prompt.contains("backlog"));
    assert!(prompt.contains("Every because.fact must exactly name a key in input.facts"));
}

fn run(home: &Path, command: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            command,
            "--fixture",
            fixture().to_str().expect("fixture path is UTF-8"),
            "--started-at",
            "2026-08-01T00:00:00Z",
        ])
        .env("OSTROM_HOME", home)
        .env(
            "OSTROM_POLICY_TRUSTED_KEYS",
            home.join("trusted-policy-keys"),
        )
        .current_dir(home)
        .output()
        .expect("run ostrom")
}

fn write_check_run(home: &Path, catalogue: &str, observations: &[(&str, &str)]) {
    let enumeration = CatalogueEnumeration {
        catalogues: vec![Catalogue {
            document: CheckDocument::from_yaml(catalogue).expect("fixture check catalogue"),
        }],
        complete: true,
    };
    // A plugin root, not OSTROM_HOME. These happen to be the same directory in
    // this fixture and the test never executes a doctor check, so passing the
    // state root worked by accident — and would break the moment a core
    // provider read a plugin asset at construction.
    let plugin_root = home;
    let registry = ActionRegistry::core(plugin_root, home).expect("core registry");
    let completed_at = "2026-08-01T00:00:00Z";
    let receipts = observations
        .iter()
        .map(|(id, observed_at)| {
            let prepared = registry
                .prepare(id, &enumeration)
                .expect("fixture check resolves");
            let observed_at = DateTime::parse_from_rfc3339(observed_at)
                .expect("fixture observation timestamp")
                .with_timezone(&Utc);
            RunnerStamp {
                resolved: prepared.resolved(),
                attempt_id: &format!("{id}-attempt"),
                observed_at,
                completed_at: DateTime::parse_from_rfc3339(completed_at)
                    .expect("fixture completion timestamp")
                    .with_timezone(&Utc),
            }
            .stamp(json!({"result_version": 1, "verdict": CheckVerdict::Pass}))
            .expect("fixture receipt")
        })
        .collect();
    let run = CheckRun {
        schema_version: CHECK_STORE_SCHEMA_VERSION,
        run_id: CheckRunId("fixture-plan-checks".to_owned()),
        completed_at: completed_at.to_owned(),
        receipts,
    };
    fs::write(
        home.join("check-runs.jsonl"),
        format!(
            "{}\n",
            serde_json::to_string(&run).expect("serialize fixture check run")
        ),
    )
    .expect("write fixture check run");
}

#[test]
fn no_goals_plan_preserves_sweep_queue_bytes_and_mechanical_steps() {
    let plan_home = tempdir().expect("plan home");
    let sweep_home = tempdir().expect("sweep home");
    configure(plan_home.path());
    configure(sweep_home.path());

    let plan = run(plan_home.path(), "plan");
    let sweep = run(sweep_home.path(), "sweep");
    assert!(
        plan.status.success(),
        "plan stderr: {}",
        String::from_utf8_lossy(&plan.stderr)
    );
    assert!(sweep.status.success());
    let planned_queue = fs::read(plan_home.path().join("queue.jsonl")).expect("plan queue");
    assert_eq!(
        planned_queue,
        fs::read(sweep_home.path().join("queue.jsonl")).expect("sweep queue"),
        "plan changed the mechanical sweep projection"
    );
    assert_eq!(
        planned_queue,
        fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/sweep-cross-org.expected.jsonl")
        )
        .expect("recorded parity queue")
    );
    let document: Value =
        serde_json::from_slice(&fs::read(plan_home.path().join("plan.json")).expect("plan output"))
            .expect("parse plan");
    assert_eq!(document["plan_version"], 1);
    assert_eq!(document["goals"], json!([]));
}

#[test]
fn unavailable_deriver_and_missing_check_are_visible_without_empty_ranking() {
    let home = tempdir().expect("plan home");
    configure(home.path());
    fs::write(
        home.path().join("goals.yaml"),
        r#"
goals_version: 1
goals:
  - id: rust-cli
    intent: ostrom runs as a product
    state: active
    serves: [{epic: example-org/example-repo#115}]
    met_when: [missing-check]
actions: []
acknowledgements: []
"#,
    )
    .expect("write goals");
    fs::write(
        home.path().join("queue.jsonl"),
        concat!(
            r##"{"id":"example-org/example-repo#10","repo":"example-org/example-repo","ref":"#10","title":"Routine maintenance","kind":"decision","mandate":{"reason":"reserved ref:#10"},"state":"approved","opened":"2026-07-01T00:00:00Z","age_days":31,"aged_out":true,"needs_judgment":true,"blocked_by":[]}"##,
            "\n",
        ),
    )
    .expect("write approved queue state");

    let output = run(home.path(), "plan");
    assert!(
        output.status.success(),
        "plan stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value =
        serde_json::from_slice(&fs::read(home.path().join("plan.json")).expect("plan output"))
            .expect("parse plan");
    assert_eq!(
        document["ranking"]["ordered"],
        json!(["example-org/example-repo#10"])
    );
    assert_eq!(document["ranking"]["computed"], json!([]));
    assert!(document["faults"].as_array().is_some_and(|faults| {
        faults
            .iter()
            .any(|fault| fault["name"] == "unresolved_check")
            && faults.iter().any(|fault| {
                fault["name"] == "assessment_evidence_unavailable"
                    && fault["detail"]
                        .as_str()
                        .is_some_and(|detail| detail.contains("missing-check"))
            })
    }));
    assert_eq!(
        document["goals"][0]["facts"]["met_when_status"][0]["state"],
        "never_run"
    );
    assert_eq!(document["goals"][0]["facts"]["met"], false);
}

#[test]
fn catalogue_checks_drive_met_state_and_keep_unknown_references_named() {
    let home = tempdir().expect("plan home");
    configure(home.path());
    let catalogue = r#"
checks_version: 1
checks:
  fresh-pass:
    uses: cmd/run
    with: {script: "exit 0"}
  never-observed:
    uses: cmd/run
    with: {script: "exit 0"}
  stale-pass:
    uses: cmd/run
    with: {script: "exit 0"}
"#;
    fs::write(home.path().join("checks.yaml"), catalogue).expect("write checks");
    fs::write(
        home.path().join("goals.yaml"),
        r#"
goals_version: 1
goals:
  - {id: fresh, intent: fresh evidence passes, state: active, met_when: [fresh-pass]}
  - {id: never, intent: absent evidence fails closed, state: active, met_when: [fresh-pass, never-observed]}
  - {id: stale, intent: expired evidence fails closed, state: active, met_when: [fresh-pass, stale-pass]}
  - {id: unknown, intent: unknown names fail closed, state: active, met_when: [not-authored]}
actions: []
acknowledgements: []
"#,
    )
    .expect("write goals");
    write_check_run(
        home.path(),
        catalogue,
        &[
            ("fresh-pass", "2026-08-01T00:00:00Z"),
            ("stale-pass", "2026-07-31T23:00:00Z"),
        ],
    );

    let output = run(home.path(), "plan");
    assert!(
        output.status.success(),
        "plan stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value =
        serde_json::from_slice(&fs::read(home.path().join("plan.json")).expect("plan output"))
            .expect("parse plan");
    let goals = document["goals"].as_array().expect("goal plans");
    let goal = |id: &str| {
        goals
            .iter()
            .find(|goal| goal["id"] == id)
            .expect("goal plan")
    };

    assert_eq!(goal("fresh")["facts"]["met"], true);
    assert_eq!(
        goal("fresh")["facts"]["met_when_status"][0]["state"],
        "passing"
    );
    assert_eq!(goal("fresh")["facts"]["basis"], "mechanical");
    assert_eq!(goal("never")["facts"]["met"], true);
    assert_eq!(
        goal("never")["facts"]["met_when_status"][1]["state"],
        "passing"
    );
    assert_eq!(goal("stale")["facts"]["met"], true);
    assert_eq!(
        goal("stale")["facts"]["met_when_status"][1]["state"],
        "passing"
    );
    assert_eq!(
        goal("unknown")["facts"]["met_when_status"][0]["fault"]["name"],
        "unresolved_check"
    );
    assert_eq!(document["sweep"]["check_runs"], 2);
}

#[test]
fn unreadable_catalogue_faults_every_reference_instead_of_resolving_a_subset() {
    let home = tempdir().expect("plan home");
    configure(home.path());
    fs::write(
        home.path().join("checks.yaml"),
        "checks_version: 1\nchecks:\n  valid-check:\n    uses: cmd/run\n    with: {script: \"exit 0\"}\n",
    )
    .expect("write user catalogue");
    fs::create_dir_all(home.path().join(".ostrom/checks.yaml"))
        .expect("create unreadable catalogue fixture");
    fs::write(
        home.path().join("goals.yaml"),
        "goals_version: 1\ngoals:\n  - id: guarded\n    intent: catalogue completeness is required\n    state: active\n    met_when: [valid-check]\n",
    )
    .expect("write goals");

    let output = run(home.path(), "plan");
    assert!(output.status.success());
    let document: Value =
        serde_json::from_slice(&fs::read(home.path().join("plan.json")).expect("plan output"))
            .expect("parse plan");
    let facts = &document["goals"][0]["facts"];
    assert_eq!(facts["met"], false);
    assert_eq!(
        facts["met_when_status"][0]["fault"]["name"],
        "check_catalog_truncated"
    );
}

#[test]
fn plan_executes_never_run_authored_criteria_before_assessment() {
    let home = tempdir().expect("plan home");
    configure(home.path());
    let marker = home.path().join("action-executed");
    let script = format!("touch {}", marker.display());
    fs::write(
        home.path().join("checks.yaml"),
        format!(
            "checks_version: 1\nchecks:\n  out-of-band:\n    uses: cmd/run\n    with:\n      script: {}\n      timeout: 1s\n",
            serde_json::to_string(&script).expect("quote fixture script")
        ),
    )
    .expect("write checks");
    fs::write(
        home.path().join("goals.yaml"),
        "goals_version: 1\ngoals:\n  - id: nonblocking\n    intent: execution stays out of band\n    state: active\n    met_when: [out-of-band]\n",
    )
    .expect("write goals");

    let output = run(home.path(), "plan");
    assert!(output.status.success());
    assert!(
        marker.exists(),
        "plan did not execute the authored criterion"
    );
    let document: Value =
        serde_json::from_slice(&fs::read(home.path().join("plan.json")).expect("plan output"))
            .expect("parse plan");
    assert_eq!(
        document["goals"][0]["facts"]["met_when_status"][0]["state"],
        "passing"
    );
    assert!(document["goals"][0]["facts"]["met"].as_bool() == Some(true));
    assert_eq!(document["sweep"]["check_runs"], 1);
}

#[cfg(unix)]
#[test]
fn plan_reruns_stale_criteria_and_the_recorded_flip_changes_the_reading() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempdir().expect("plan home");
    configure(home.path());
    let switch = home.path().join("criterion-switch");
    fs::write(&switch, "pass").expect("write criterion switch");
    let flip_script = format!("test \"$(cat {})\" = pass", switch.display());
    fs::write(
        home.path().join("checks.yaml"),
        format!(
            concat!(
                "checks_version: 1\n",
                "checks:\n",
                "  anchor:\n",
                "    uses: cmd/run\n",
                "    with: {{script: \"exit 1\"}}\n",
                "  flip:\n",
                "    uses: cmd/run\n",
                "    with: {{script: {}, fresh_for: 1s}}\n",
            ),
            serde_json::to_string(&flip_script).expect("quote flip script")
        ),
    )
    .expect("write checks");
    fs::write(
        home.path().join("goals.yaml"),
        r#"
goals_version: 1
goals:
  - id: criteria-change
    intent: recorded criteria determine the reading
    state: active
    serves: [{epic: example-org/example-repo#115}]
    met_when: [anchor, flip]
actions: []
acknowledgements: []
"#,
    )
    .expect("write goals");
    fs::write(
        home.path().join("queue.jsonl"),
        concat!(
            r##"{"id":"example-org/example-repo#10","repo":"example-org/example-repo","ref":"#10","title":"Routine maintenance","kind":"decision","mandate":{"reason":"reserved ref:#10"},"state":"approved","opened":"2026-07-01T00:00:00Z","age_days":31,"aged_out":true,"needs_judgment":true,"blocked_by":[]}"##,
            "\n",
        ),
    )
    .expect("write approved queue state");
    let mut fixture_value: Value =
        serde_json::from_slice(&fs::read(fixture()).expect("read fixture")).expect("fixture JSON");
    fixture_value["repositories"][0]["issues"][0]["epic"] = json!("example-org/example-repo#115");
    let plan_fixture = home.path().join("plan-fixture.json");
    fs::write(
        &plan_fixture,
        serde_json::to_vec(&fixture_value).expect("serialize fixture"),
    )
    .expect("write plan fixture");
    let deriver = home.path().join("criteria-deriver");
    fs::write(
        &deriver,
        concat!(
            "#!/bin/sh\n",
            "input=$(cat)\n",
            "case \"$input\" in\n",
            "  *'\"met_when.flip.state\":\"pass\"'*) reading=on-track ;;\n",
            "  *'\"met_when.flip.state\":\"fail\"'*) reading=off-track ;;\n",
            "  *) exit 9 ;;\n",
            "esac\n",
            "printf '{\"goal\":\"criteria-change\",\"reading\":\"%s\",\"because\":[{\"fact\":\"met_when.flip.state\",\"detail\":\"the recorded flip verdict changed\"}]}\\n' \"$reading\"\n",
        ),
    )
    .expect("write assessment deriver");
    fs::set_permissions(&deriver, fs::Permissions::from_mode(0o700)).expect("deriver mode");

    let run_plan_at = |started_at: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
            .args([
                "plan",
                "--fixture",
                plan_fixture.to_str().expect("fixture path"),
                "--started-at",
                started_at,
            ])
            .env("OSTROM_HOME", home.path())
            .env(
                "OSTROM_POLICY_TRUSTED_KEYS",
                home.path().join("trusted-policy-keys"),
            )
            .env("OSTROM_PLAN_DERIVER", &deriver)
            .current_dir(home.path())
            .output()
            .expect("run plan");
        assert!(
            output.status.success(),
            "plan stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(
            &fs::read(home.path().join("plan.json")).expect("plan output"),
        )
        .expect("parse plan")
    };

    let passing = run_plan_at("2026-08-01T00:00:00Z");
    assert_eq!(passing["goals"][0]["assessment"]["reading"], "on-track");
    assert_eq!(
        passing["goals"][0]["facts"]["met_when_status"][1]["state"],
        "passing"
    );

    fs::write(&switch, "fail").expect("flip criterion");
    let failing = run_plan_at("2026-08-01T00:00:02Z");
    assert_eq!(failing["goals"][0]["assessment"]["reading"], "off-track");
    assert_eq!(
        failing["goals"][0]["facts"]["met_when_status"][1]["state"],
        "failing"
    );

    let runs = fs::read_to_string(home.path().join("check-runs.jsonl"))
        .expect("read criteria journal")
        .lines()
        .map(|line| serde_json::from_str::<CheckRun>(line).expect("decode criteria run"))
        .collect::<Vec<_>>();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].receipts.len(), 2);
    assert_eq!(runs[1].receipts.len(), 1, "only stale criteria rerun");
    assert_eq!(runs[1].receipts[0].check, "flip");
    assert_eq!(runs[1].receipts[0].verdict, Some(CheckVerdict::Fail));
}

#[cfg(unix)]
#[test]
fn plan_refuses_to_assess_when_execution_produces_no_verdict() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempdir().expect("plan home");
    let plan_fixture = configure_assessor_case(home.path());
    fs::write(
        home.path().join("checks.yaml"),
        "checks_version: 1\nchecks:\n  unavailable:\n    uses: cmd/run\n    with: {script: \"sleep 1\", timeout: 10ms}\n",
    )
    .expect("write checks");
    fs::write(
        home.path().join("goals.yaml"),
        r#"
goals_version: 1
goals:
  - id: rust-cli
    intent: ostrom runs as a product
    state: active
    serves: [{epic: example-org/example-repo#115}]
    met_when: [unavailable]
actions: []
acknowledgements: []
"#,
    )
    .expect("write goals");
    let invoked = home.path().join("assessor-invoked");
    let deriver = home.path().join("must-not-run-deriver");
    fs::write(
        &deriver,
        format!(
            "#!/bin/sh\ntouch {}\nprintf '%s\\n' '{{\"goal\":\"rust-cli\",\"reading\":\"off-track\",\"because\":[{{\"fact\":\"goal.met\",\"detail\":\"fixture\"}}]}}'\n",
            invoked.display()
        ),
    )
    .expect("write deriver");
    fs::set_permissions(&deriver, fs::Permissions::from_mode(0o700)).expect("deriver mode");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "plan",
            "--fixture",
            plan_fixture.to_str().expect("fixture path"),
            "--started-at",
            "2026-08-01T00:00:00Z",
        ])
        .env("OSTROM_HOME", home.path())
        .env(
            "OSTROM_POLICY_TRUSTED_KEYS",
            home.path().join("trusted-policy-keys"),
        )
        .env("OSTROM_PLAN_DERIVER", &deriver)
        .current_dir(home.path())
        .output()
        .expect("run plan");
    assert!(
        output.status.success(),
        "plan stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !invoked.exists(),
        "assessor ran without a criterion verdict"
    );
    let document: Value =
        serde_json::from_slice(&fs::read(home.path().join("plan.json")).expect("plan output"))
            .expect("parse plan");
    let status = &document["goals"][0]["facts"]["met_when_status"][0];
    assert_eq!(status["state"], "inconclusive");
    assert_eq!(status["rendered"], "inconclusive");
    assert!(status["fault"].is_null());
    assert!(document["goals"][0]["assessment"].is_null());
    assert!(document["faults"].as_array().is_some_and(|faults| {
        faults
            .iter()
            .any(|fault| fault["name"] == "assessment_evidence_unavailable")
    }));

    let run: CheckRun = serde_json::from_str(
        fs::read_to_string(home.path().join("check-runs.jsonl"))
            .expect("read check journal")
            .lines()
            .next()
            .expect("check run"),
    )
    .expect("decode check run");
    assert_eq!(run.receipts[0].verdict, Some(CheckVerdict::Inconclusive));
    assert_eq!(run.receipts[0].error, None);
}

#[cfg(unix)]
#[test]
fn cited_stub_assessment_promotes_only_the_authorized_next_milestone() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempdir().expect("plan home");
    configure(home.path());
    fs::write(
        home.path().join("goals.yaml"),
        r#"
goals_version: 1
goals:
  - id: rust-cli
    intent: ostrom runs as a product
    state: active
    serves: [{epic: example-org/example-repo#115}]
    met_when: []
actions: []
acknowledgements: []
"#,
    )
    .expect("write goals");
    fs::write(
        home.path().join("queue.jsonl"),
        concat!(
            r##"{"id":"example-org/example-repo#10","repo":"example-org/example-repo","ref":"#10","title":"Routine maintenance","kind":"decision","mandate":{"reason":"reserved ref:#10"},"state":"approved","opened":"2026-07-01T00:00:00Z","age_days":31,"aged_out":true,"needs_judgment":true,"blocked_by":[]}"##,
            "\n",
        ),
    )
    .expect("write approved queue state");
    let mut fixture_value: Value =
        serde_json::from_slice(&fs::read(fixture()).expect("read fixture")).expect("fixture JSON");
    fixture_value["repositories"][0]["issues"][0]["epic"] = json!("example-org/example-repo#115");
    let plan_fixture = home.path().join("plan-fixture.json");
    fs::write(
        &plan_fixture,
        serde_json::to_vec(&fixture_value).expect("serialize fixture"),
    )
    .expect("write plan fixture");
    let deriver = home.path().join("deriver");
    fs::write(
        &deriver,
        concat!(
            "#!/usr/bin/env bash\n",
            "set -eu\n",
            "input=\"$(cat)\"\n",
            "test \"$(jq -r '.goal' <<<\"$input\")\" = rust-cli\n",
            "test \"$(jq -r 'has(\"facts\") and (has(\"backlog\") | not)' <<<\"$input\")\" = true\n",
            "printf '%s\\n' '{\"goal\":\"rust-cli\",\"reading\":\"off-track\",\"because\":[{\"fact\":\"next.dispatchable\",\"detail\":\"the next milestone is authorized and unselected\"}]}'\n",
        ),
    )
    .expect("write deriver");
    fs::set_permissions(&deriver, fs::Permissions::from_mode(0o700)).expect("deriver mode");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "plan",
            "--fixture",
            plan_fixture.to_str().expect("fixture path"),
            "--started-at",
            "2026-08-01T00:00:00Z",
        ])
        .env("OSTROM_HOME", home.path())
        .env(
            "OSTROM_POLICY_TRUSTED_KEYS",
            home.path().join("trusted-policy-keys"),
        )
        .env("OSTROM_PLAN_DERIVER", &deriver)
        .current_dir(home.path())
        .output()
        .expect("run plan");
    assert!(
        output.status.success(),
        "plan stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value =
        serde_json::from_slice(&fs::read(home.path().join("plan.json")).expect("plan output"))
            .expect("parse plan");
    assert_eq!(
        document["goals"][0]["assessment"]["consequence"]["promote"],
        json!(["example-org/example-repo#10"])
    );
    assert_eq!(
        document["ranking"]["computed"],
        json!(["example-org/example-repo#10"])
    );
}

#[cfg(unix)]
#[test]
fn claude_harness_satisfies_the_cited_contract_and_is_the_default() {
    let home = tempdir().expect("plan home");
    let stub = write_harness_stub(home.path());
    let assessment = r#"{"goal":"rust-cli","reading":"off-track","because":[{"fact":"next.dispatchable","detail":"the next milestone is authorized and unselected"}]}"#;
    let envelope = format!(r#"{{"structured_output":{assessment}}}"#);
    let (document, arguments, prompt) =
        run_named_assessor(home.path(), "claude", &stub, &envelope, true);
    assert_harness_contract(&document, &arguments, &prompt, "--max-budget-usd\n1");
    assert!(arguments.iter().any(|argument| argument == "--max-turns"));
    assert!(arguments.iter().any(|argument| argument == "--json-schema"));
}

#[cfg(unix)]
#[test]
fn codex_harness_satisfies_the_cited_contract() {
    let home = tempdir().expect("plan home");
    let stub = write_harness_stub(home.path());
    let assessment = r#"{"goal":"rust-cli","reading":"off-track","because":[{"fact":"next.dispatchable","detail":"the next milestone is authorized and unselected"}]}"#;
    let (document, arguments, prompt) =
        run_named_assessor(home.path(), "codex", &stub, assessment, false);
    assert_harness_contract(&document, &arguments, &prompt, "model_context_window=25000");
    assert!(arguments.iter().any(|argument| argument == "--ephemeral"));
    assert!(arguments.iter().any(|argument| argument == "shell_tool"));
    assert!(arguments.iter().any(|argument| argument == "read-only"));
}

#[cfg(unix)]
#[test]
fn copilot_harness_satisfies_the_cited_contract() {
    let home = tempdir().expect("plan home");
    let stub = write_harness_stub(home.path());
    let assessment = r#"{"goal":"rust-cli","reading":"off-track","because":[{"fact":"next.dispatchable","detail":"the next milestone is authorized and unselected"}]}"#;
    let (document, arguments, prompt) =
        run_named_assessor(home.path(), "copilot", &stub, assessment, false);
    assert_harness_contract(&document, &arguments, &prompt, "--max-ai-credits=1");
    assert!(
        arguments
            .iter()
            .any(|argument| argument == "--available-tools=")
    );
    assert!(
        arguments
            .iter()
            .any(|argument| argument == "--disable-builtin-mcps")
    );
}

#[cfg(unix)]
fn assert_uncited_harness_is_refused(harness: &str) {
    let home = tempdir().expect("plan home");
    let stub = write_harness_stub(home.path());
    let assessment = r#"{"goal":"rust-cli","reading":"off-track","because":[]}"#;
    let (document, _, _) = run_named_assessor(home.path(), harness, &stub, assessment, false);
    assert_eq!(document["ranking"]["computed"], json!([]));
    assert_eq!(
        document["ranking"]["ordered"],
        json!(["example-org/example-repo#10"])
    );
    assert!(document["goals"][0]["assessment"].is_null());
    assert!(document["faults"].as_array().is_some_and(|faults| {
        faults.iter().any(|fault| {
            fault["name"] == "assessment_invalid_output"
                && fault["detail"] == "assessment must cite at least one computed fact"
        })
    }));
}

#[cfg(unix)]
#[test]
fn claude_harness_refuses_an_uncited_reading() {
    assert_uncited_harness_is_refused("claude");
}

#[cfg(unix)]
#[test]
fn codex_harness_refuses_an_uncited_reading() {
    assert_uncited_harness_is_refused("codex");
}

#[cfg(unix)]
#[test]
fn copilot_harness_refuses_an_uncited_reading() {
    assert_uncited_harness_is_refused("copilot");
}

#[cfg(unix)]
#[test]
fn configured_missing_harness_has_a_distinct_named_fault() {
    let home = tempdir().expect("plan home");
    let plan_fixture = configure_assessor_case(home.path());
    let missing = home.path().join("missing-placeholder-claude");
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "plan",
            "--assessor",
            "claude",
            "--fixture",
            plan_fixture.to_str().expect("fixture path"),
            "--started-at",
            "2026-08-01T00:00:00Z",
        ])
        .env("OSTROM_HOME", home.path())
        .env(
            "OSTROM_POLICY_TRUSTED_KEYS",
            home.path().join("trusted-policy-keys"),
        )
        .env_remove("OSTROM_PLAN_DERIVER")
        .env("CLAUDE_BIN", &missing)
        .current_dir(home.path())
        .output()
        .expect("run missing assessor");
    assert!(output.status.success());
    let document: Value =
        serde_json::from_slice(&fs::read(home.path().join("plan.json")).expect("plan output"))
            .expect("parse plan");
    assert!(document["faults"].as_array().is_some_and(|faults| {
        faults.iter().any(|fault| {
            fault["name"] == "assessment_harness_unavailable"
                && fault["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.contains("claude assessor is configured"))
        })
    }));
    assert_eq!(document["ranking"]["computed"], json!([]));
}

#[cfg(unix)]
#[test]
fn named_assessment_stops_at_the_plan_goal_ceiling() {
    let home = tempdir().expect("plan home");
    let plan_fixture = configure_assessor_case(home.path());
    let stub = write_harness_stub(home.path());
    let goals = (0..21)
        .map(|index| {
            format!(
                "  - id: goal-{index}\n    intent: placeholder intent {index}\n    state: active\n    serves: [{{epic: example-org/example-repo#115}}]\n    met_when: []\n"
            )
        })
        .collect::<String>();
    fs::write(
        home.path().join("goals.yaml"),
        format!("goals_version: 1\ngoals:\n{goals}actions: []\nacknowledgements: []\n"),
    )
    .expect("write bounded goals");
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "plan",
            "--assessor",
            "claude",
            "--fixture",
            plan_fixture.to_str().expect("fixture path"),
            "--started-at",
            "2026-08-01T00:00:00Z",
        ])
        .env("OSTROM_HOME", home.path())
        .env(
            "OSTROM_POLICY_TRUSTED_KEYS",
            home.path().join("trusted-policy-keys"),
        )
        .env_remove("OSTROM_PLAN_DERIVER")
        .env("CLAUDE_BIN", &stub)
        .env("OSTROM_STUB_ARGS", home.path().join("assessor-args"))
        .env("OSTROM_STUB_INPUT", home.path().join("assessor-input"))
        .env(
            "OSTROM_STUB_OUTPUT",
            r#"{"goal":"goal-0","reading":"off-track","because":[{"fact":"next.dispatchable","detail":"placeholder cited detail"}]}"#,
        )
        .current_dir(home.path())
        .output()
        .expect("run bounded assessor");
    assert!(output.status.success());
    let document: Value =
        serde_json::from_slice(&fs::read(home.path().join("plan.json")).expect("plan output"))
            .expect("parse plan");
    assert!(document["faults"].as_array().is_some_and(|faults| {
        faults.iter().any(|fault| {
            fault["goal"] == "goal-20"
                && fault["name"] == "assessment_budget_exhausted"
                && fault["detail"] == "claude assessor reached the 20-goal plan ceiling"
        })
    }));
}

#[test]
fn builder_uses_a_fresh_plan_order_after_the_principal_prefix() {
    let fixture = tempdir().expect("selector home");
    let data = fixture.path().join("ostrom");
    fs::create_dir(&data).expect("data dir");
    let roster = r#"
provider: file
cadence_hours: 1
stuck_after_days: 7
search_roots: []
hold_labels: []
work_ranking: []
bounce_all: []
projects:
  - repo: example-org/example-repo
    delegated:
      - type:fix
    excluded: []
    reserved: []
    default: delegated
    paused: false
    bounce: []
"#;
    fs::write(data.join("mandates.yaml"), roster).expect("mandates");
    let rows = [
        json!({
            "id": "example-org/example-repo#1", "repo": "example-org/example-repo",
            "ref": "#1", "title": "First", "kind": "moved",
            "mandate": {"reason": "delegated"}, "state": "pending",
            "opened": "2026-07-01T00:00:00Z", "blocked_by": []
        }),
        json!({
            "id": "example-org/example-repo#2", "repo": "example-org/example-repo",
            "ref": "#2", "title": "Second", "kind": "moved",
            "mandate": {"reason": "delegated"}, "state": "pending",
            "opened": "2026-07-02T00:00:00Z", "blocked_by": []
        }),
    ];
    let queue = rows
        .iter()
        .map(|row| serde_json::to_string(row).expect("row JSON"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(data.join("queue.jsonl"), queue).expect("queue");
    fs::write(
        data.join("state.json"),
        serde_json::to_vec(&json!({
            "version": 2,
            "dependency_graph": {
                "graph_version": 1,
                "configured_repositories": ["example-org/example-repo"],
                "nodes": [
                    {"id":"example-org/example-repo#1","open":true,"dependencies":[],"unsatisfied":[],"children":[],"dispatchable":true,"unblocking_power":10},
                    {"id":"example-org/example-repo#2","open":true,"dependencies":[],"unsatisfied":[],"children":[],"dispatchable":true,"unblocking_power":0}
                ],
                "edges": [],
                "faults": []
            }
        }))
        .expect("state JSON"),
    )
    .expect("state");
    fs::write(
        data.join("plan.json"),
        serde_json::to_vec(&json!({
            "plan_version": 1,
            "queue_basis": [
                {"id":"example-org/example-repo#1","opened":"2026-07-01T00:00:00Z","kind":"moved","state":"pending","blocked_by":[],"graph_dispatchable":true,"unblocking_power":10},
                {"id":"example-org/example-repo#2","opened":"2026-07-02T00:00:00Z","kind":"moved","state":"pending","blocked_by":[],"graph_dispatchable":true,"unblocking_power":0}
            ],
            "ranking": {
                "work_ranking": [],
                "ordered": ["example-org/example-repo#2", "example-org/example-repo#1"]
            }
        }))
        .expect("plan JSON"),
    )
    .expect("plan");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root");
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["select-work", "list"])
        .env("CLAUDE_CONFIG_DIR", fixture.path())
        .env("CLAUDE_PLUGIN_ROOT", root.join("plugins/ostrom"))
        .current_dir(fixture.path())
        .output()
        .expect("run selector");
    assert!(
        output.status.success(),
        "selector stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first: Value = serde_json::from_slice(
        output
            .stdout
            .split(|byte| *byte == b'\n')
            .find(|line| !line.is_empty())
            .expect("first row"),
    )
    .expect("row JSON");
    assert_eq!(first["id"], "example-org/example-repo#2");
}
