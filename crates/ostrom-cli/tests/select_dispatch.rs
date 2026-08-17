use std::{fs, process::Command};

use serde_json::json;
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
        .env("MANDATE_TRACE_TIME", "2026-08-17T00:00:00Z")
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
fn known_empty_selection_is_successful_and_has_no_rows() {
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
