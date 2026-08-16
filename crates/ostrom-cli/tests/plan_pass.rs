use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::{Value, json};
use tempfile::tempdir;

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
        .current_dir(home)
        .output()
        .expect("run ostrom")
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
    assert!(document["faults"].as_array().is_some_and(|faults| {
        faults
            .iter()
            .any(|fault| fault["name"] == "unresolved_check")
            && faults
                .iter()
                .any(|fault| fault["name"] == "assessment_unavailable")
    }));
    assert_eq!(
        document["goals"][0]["facts"]["met_when_status"][0]["state"],
        "never_run"
    );
    assert_eq!(document["goals"][0]["facts"]["met"], false);
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
        data.join("plan.json"),
        serde_json::to_vec(&json!({
            "plan_version": 1,
            "queue_basis": [
                {"id":"example-org/example-repo#1","opened":"2026-07-01T00:00:00Z","kind":"moved","state":"pending","blocked_by":[]},
                {"id":"example-org/example-repo#2","opened":"2026-07-02T00:00:00Z","kind":"moved","state":"pending","blocked_by":[]}
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
    let output = Command::new("bash")
        .args([
            root.join("plugins/ostrom/scripts/select-work.sh"),
            "list".into(),
        ])
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
