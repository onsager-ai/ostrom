use std::{fs, process::Command};

use serde_json::{Value, json};
use tempfile::tempdir;

fn write_selection_fixture(root: &std::path::Path, padding: usize) {
    fs::write(
        root.join("mandates.yaml"),
        r#"provider: file
cadence_hours: 1
stuck_after_days: 7
work_ranking:
  - placeholder-org/alpha#2
bounce_all: []
projects:
  - repo: placeholder-org/alpha
    delegated: []
    excluded: []
    reserved: []
    default: delegated
    paused: false
    bounce: []
"#,
    )
    .expect("write config");
    fs::write(
        root.join("queue.jsonl"),
        concat!(
            "{\"id\":\"placeholder-org/alpha#1\",\"repo\":\"placeholder-org/alpha\",\"ref\":\"#1\",\"title\":\"Older placeholder\",\"kind\":\"moved\",\"mandate\":{\"reason\":\"placeholder\"},\"state\":\"pending\",\"opened\":\"2026-01-01T00:00:00Z\",\"blocked_by\":[]}\n",
            "{\"id\":\"placeholder-org/alpha#2\",\"repo\":\"placeholder-org/alpha\",\"ref\":\"#2\",\"title\":\"Ranked placeholder\",\"kind\":\"stuck\",\"mandate\":{\"reason\":\"placeholder\"},\"state\":\"pending\",\"opened\":\"2026-02-01T00:00:00Z\",\"blocked_by\":[]}\n",
            "{\"id\":\"placeholder-org/alpha#3\",\"repo\":\"placeholder-org/alpha\",\"ref\":\"#3\",\"title\":\"Gated placeholder\",\"kind\":\"moved\",\"mandate\":{\"reason\":\"placeholder\"},\"state\":\"pending\",\"opened\":\"2025-12-01T00:00:00Z\",\"blocked_by\":[\"placeholder-org/alpha#1\"]}\n"
        ),
    )
    .expect("write queue");
    let state = json!({
        "version": 2,
        "work_ranking": ["placeholder-org/alpha#2"],
        "work_ranking_faults": [],
        "production_scale_padding": "x".repeat(padding),
        "dependency_graph": {
            "graph_version": 1,
            "configured_repositories": ["placeholder-org/alpha"],
            "nodes": [
                {"id":"placeholder-org/alpha#1","open":true,"dependencies":[],"unsatisfied":[],"children":[],"dispatchable":true,"unblocking_power":1},
                {"id":"placeholder-org/alpha#2","open":true,"dependencies":[],"unsatisfied":[],"children":[],"dispatchable":true,"unblocking_power":0},
                {"id":"placeholder-org/alpha#3","open":true,"dependencies":["placeholder-org/alpha#1"],"unsatisfied":["placeholder-org/alpha#1"],"children":[],"dispatchable":false,"unblocking_power":0}
            ],
            "edges": [
                {"dependency":"placeholder-org/alpha#1","item":"placeholder-org/alpha#3","sources":["body"]}
            ],
            "faults": []
        }
    });
    fs::write(
        root.join("state.json"),
        serde_json::to_vec(&state).expect("state JSON"),
    )
    .expect("write state");
}

fn write_valid_selection_plan(root: &std::path::Path) {
    fs::write(
        root.join("plan.json"),
        serde_json::to_vec(&json!({
            "plan_version": 1,
            "queue_basis": [
                {"id":"placeholder-org/alpha#1","opened":"2026-01-01T00:00:00Z","kind":"moved","state":"pending","blocked_by":[],"graph_dispatchable":true,"unblocking_power":1},
                {"id":"placeholder-org/alpha#2","opened":"2026-02-01T00:00:00Z","kind":"stuck","state":"pending","blocked_by":[],"graph_dispatchable":true,"unblocking_power":0},
                {"id":"placeholder-org/alpha#3","opened":"2025-12-01T00:00:00Z","kind":"moved","state":"pending","blocked_by":["placeholder-org/alpha#1"],"graph_dispatchable":false,"unblocking_power":0}
            ],
            "ranking": {
                "work_ranking": ["placeholder-org/alpha#2"],
                "ordered": ["placeholder-org/alpha#1", "placeholder-org/alpha#2"]
            }
        }))
        .expect("plan JSON"),
    )
    .expect("write plan");
}

fn read_trace(root: &std::path::Path) -> Vec<Value> {
    fs::read_to_string(root.join("sprint.jsonl"))
        .expect("read trace")
        .lines()
        .map(|line| serde_json::from_str(line).expect("trace row JSON"))
        .collect()
}

#[test]
fn selection_matches_recorded_shell_bytes() {
    let fixture = tempdir().expect("fixture");
    write_selection_fixture(fixture.path(), 0);
    let binary = env!("CARGO_BIN_EXE_ostrom");
    let list = Command::new(binary)
        .args(["select-work", "list"])
        .env("OSTROM_HOME", fixture.path())
        .current_dir(fixture.path())
        .output()
        .expect("list selection");
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert_eq!(
        list.stdout,
        include_bytes!("fixtures/select-work-shell-list.expected.jsonl")
    );
    assert!(list.stderr.is_empty());

    let selected = Command::new(binary)
        .args([
            "select-work",
            "select",
            "builder-placeholder-wake1",
            "placeholder-org/alpha#2",
        ])
        .env("OSTROM_HOME", fixture.path())
        .current_dir(fixture.path())
        .output()
        .expect("select one");
    assert!(selected.status.success());
    assert_eq!(
        selected.stdout,
        include_bytes!("fixtures/select-work-shell-select.expected.json")
    );
    assert!(selected.stderr.is_empty());
}

#[test]
fn selection_usage_matches_the_accepted_argument_shapes() {
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["select-work", "list", "placeholder-owner"])
        .output()
        .expect("invalid list selection");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"usage: ostrom select-work list | select <owner> [already-attempted-id ...]\n"
    );
}

#[test]
fn production_scale_state_is_read_from_file_and_selects_work() {
    let fixture = tempdir().expect("fixture");
    write_selection_fixture(fixture.path(), 700 * 1024);
    assert!(
        fs::metadata(fixture.path().join("state.json"))
            .unwrap()
            .len()
            > 600 * 1024
    );
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["select-work", "list"])
        .env("OSTROM_HOME", fixture.path())
        .current_dir(fixture.path())
        .output()
        .expect("large selection");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        include_bytes!("fixtures/select-work-shell-list.expected.jsonl")
    );
}

#[test]
fn selection_fault_cannot_be_reported_as_a_successful_empty_result() {
    let fixture = tempdir().expect("fixture");
    write_selection_fixture(fixture.path(), 0);
    fs::write(fixture.path().join("state.json"), "{malformed").expect("break state");
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["select-work", "list"])
        .env("OSTROM_HOME", fixture.path())
        .current_dir(fixture.path())
        .output()
        .expect("faulted selection");
    assert_eq!(output.status.code(), Some(4));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("mandate selection: cannot read"));
}

#[test]
fn selection_io_fault_is_named_and_nonzero() {
    let fixture = tempdir().expect("fixture");
    write_selection_fixture(fixture.path(), 0);
    fs::remove_file(fixture.path().join("state.json")).expect("remove state file");
    fs::create_dir(fixture.path().join("state.json")).expect("replace state with directory");
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["select-work", "list"])
        .env("OSTROM_HOME", fixture.path())
        .current_dir(fixture.path())
        .output()
        .expect("faulted selection");
    assert_eq!(output.status.code(), Some(4));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("mandate selection: cannot read"));
}

#[test]
fn known_empty_selection_is_successful_and_has_no_output_rows() {
    let fixture = tempdir().expect("fixture");
    write_selection_fixture(fixture.path(), 0);
    fs::write(fixture.path().join("queue.jsonl"), "").expect("empty queue");
    fs::write(
        fixture.path().join("mandates.yaml"),
        r#"provider: file
cadence_hours: 1
stuck_after_days: 7
bounce_all: []
projects:
  - repo: placeholder-org/alpha
    delegated: []
    excluded: []
    reserved: []
    default: delegated
    paused: false
    bounce: []
"#,
    )
    .expect("config without ranking");
    let mut state: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.path().join("state.json")).expect("read state"))
            .expect("parse state");
    state["dependency_graph"]["nodes"] = json!([]);
    state["dependency_graph"]["edges"] = json!([]);
    fs::write(
        fixture.path().join("state.json"),
        serde_json::to_vec(&state).expect("state JSON"),
    )
    .expect("write empty state");
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["select-work", "list"])
        .env("OSTROM_HOME", fixture.path())
        .current_dir(fixture.path())
        .output()
        .expect("empty selection");
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let rows = read_trace(fixture.path());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["kind"], "plan-selection");
    assert_eq!(rows[0]["fact"]["action"], "list");
    assert_eq!(rows[0]["fact"]["plan_status"], "absent");
    for field in ["selected", "repo", "ref"] {
        assert!(rows[0]["fact"].get(field).is_none(), "unexpected {field}");
    }
}

#[test]
fn rejected_plan_diagnostic_and_trace_name_the_clause() {
    for (clause, plan) in [
        (
            "queue_basis",
            serde_json::to_vec(&json!({
                "plan_version": 1,
                "queue_basis": []
            }))
            .expect("queue-basis plan JSON"),
        ),
        ("malformed_json", b"{malformed".to_vec()),
    ] {
        let fixture = tempdir().expect("fixture");
        write_selection_fixture(fixture.path(), 0);
        fs::write(fixture.path().join("plan.json"), plan).expect("write rejected plan");

        let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
            .args(["select-work", "list"])
            .env("OSTROM_HOME", fixture.path())
            .env("MANDATE_TRACE_TIME", "2026-08-21T00:00:00Z")
            .current_dir(fixture.path())
            .output()
            .expect("list with rejected plan");

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stderr).expect("UTF-8 diagnostic"),
            format!("mandate selection: plan.json rejected ({clause}); using mechanical ranking\n")
        );
        let rows = read_trace(fixture.path());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["kind"], "plan-selection");
        assert_eq!(rows[0]["fact"]["action"], "list");
        assert_eq!(rows[0]["fact"]["plan_status"], "rejected");
        assert_eq!(rows[0]["fact"]["plan_rejection_clause"], clause);
    }
}

#[test]
fn list_and_empty_select_record_plan_application_without_a_selected_item() {
    let fixture = tempdir().expect("fixture");
    write_selection_fixture(fixture.path(), 0);
    write_valid_selection_plan(fixture.path());
    let binary = env!("CARGO_BIN_EXE_ostrom");

    let list = Command::new(binary)
        .args(["select-work", "list"])
        .env("OSTROM_HOME", fixture.path())
        .env("MANDATE_TRACE_TIME", "2026-08-21T00:00:00Z")
        .current_dir(fixture.path())
        .output()
        .expect("list selection");
    assert!(list.status.success());
    assert!(list.stderr.is_empty());

    let empty = Command::new(binary)
        .args([
            "select-work",
            "select",
            "builder-placeholder-wake1",
            "placeholder-org/alpha#1",
            "placeholder-org/alpha#2",
        ])
        .env("OSTROM_HOME", fixture.path())
        .env("MANDATE_TRACE_TIME", "2026-08-21T00:00:01Z")
        .current_dir(fixture.path())
        .output()
        .expect("empty selection");
    assert_eq!(empty.status.code(), Some(3));
    assert!(empty.stdout.is_empty());
    assert!(empty.stderr.is_empty());

    let rows = read_trace(fixture.path());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["kind"], "plan-selection");
    assert_eq!(rows[0]["fact"]["action"], "list");
    assert_eq!(rows[0]["fact"]["plan_status"], "applied");
    assert_eq!(rows[1]["kind"], "plan-selection");
    assert_eq!(rows[1]["fact"]["owner"], "builder-placeholder-wake1");
    assert_eq!(rows[1]["fact"]["action"], "select");
    assert_eq!(rows[1]["fact"]["outcome"], "empty");
    assert_eq!(rows[1]["fact"]["plan_status"], "applied");
    for field in ["selected", "repo", "ref"] {
        assert!(rows[1]["fact"].get(field).is_none(), "unexpected {field}");
    }
}

#[test]
fn principal_state_transition_changes_the_selection_candidate_immediately() {
    let fixture = tempdir().expect("fixture");
    write_selection_fixture(fixture.path(), 0);
    let queue_path = fixture.path().join("queue.jsonl");
    let pending = fs::read_to_string(&queue_path)
        .expect("read queue")
        .replace(
            "\"kind\":\"moved\",\"mandate\":{\"reason\":\"placeholder\"},\"state\":\"pending\"",
            "\"kind\":\"decision\",\"mandate\":{\"reason\":\"placeholder\"},\"state\":\"pending\"",
        );
    fs::write(&queue_path, pending).expect("make first item await principal");

    let run_list = || {
        Command::new(env!("CARGO_BIN_EXE_ostrom"))
            .args(["select-work", "list"])
            .env("OSTROM_HOME", fixture.path())
            .current_dir(fixture.path())
            .output()
            .expect("list selection")
    };
    let before = run_list();
    assert!(before.status.success());
    assert!(!String::from_utf8_lossy(&before.stdout).contains("placeholder-org/alpha#1"));

    let approved = fs::read_to_string(&queue_path)
        .expect("read pending queue")
        .replace("\"state\":\"pending\"", "\"state\":\"approved\"");
    fs::write(&queue_path, approved).expect("approve first item");
    let after = run_list();
    assert!(after.status.success());
    assert!(String::from_utf8_lossy(&after.stdout).contains("placeholder-org/alpha#1"));
}

#[test]
fn dispatch_usage_and_invalid_order_match_recorded_shell_corpus() {
    let binary = env!("CARGO_BIN_EXE_ostrom");
    let usage = Command::new(binary)
        .arg("dispatch")
        .output()
        .expect("usage refusal");
    let invalid_path = std::path::Path::new("/tmp/ostrom-dispatch-parity-invalid.json");
    fs::write(invalid_path, "{}\n").expect("invalid order fixture");
    let invalid = Command::new(binary)
        .args(["dispatch", invalid_path.to_str().unwrap()])
        .output()
        .expect("invalid order refusal");
    let recording = format!(
        "usage status={}\nusage stderr={}invalid status={}\ninvalid stderr={}",
        usage.status.code().unwrap(),
        String::from_utf8_lossy(&usage.stderr),
        invalid.status.code().unwrap(),
        String::from_utf8_lossy(&invalid.stderr)
    );
    assert_eq!(
        recording.as_bytes(),
        include_bytes!("fixtures/dispatch-shell.expected.txt")
    );
}
