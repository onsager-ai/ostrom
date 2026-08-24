use std::{fs, path::Path, process::Command};

use serde_json::{Value, json};
use tempfile::tempdir;

mod support;

const REPOSITORY: &str = "placeholder-org/alpha";
const ROSTER: &str = r#"
provider: file
cadence_hours: 1
stuck_after_days: 7
search_roots: []
hold_labels: []
bounce_all: []
projects:
  - repo: placeholder-org/alpha
    delegated: []
    excluded: []
    reserved: []
    default: excluded
    paused: false
    bounce: []
"#;

fn issue(number: u64) -> Value {
    json!({
        "number": number,
        "title": format!("Placeholder item {number}"),
        "state": "OPEN",
        "body": "",
        "labels": [],
        "createdAt": "2026-08-01T00:00:00Z",
        "updatedAt": "2026-08-24T00:00:00Z",
    })
}

fn install_policy(home: &Path) -> std::path::PathBuf {
    fs::write(home.join("mandates.yaml"), ROSTER).expect("write synthetic roster");
    fs::write(home.join("ostrom.yaml"), "manifest_version: 1\n")
        .expect("write synthetic policy manifest");
    support::sign_manifest(&home.join("ostrom.yaml"))
}

fn queue(home: &Path) -> Vec<Value> {
    fs::read_to_string(home.join("queue.jsonl"))
        .expect("read queue")
        .lines()
        .map(|line| serde_json::from_str(line).expect("parse queue row"))
        .collect()
}

#[test]
fn real_artifact_shapes_report_337_and_422_without_flagging_unimplemented_items() {
    let home = tempdir().expect("temporary OSTROM_HOME");
    let trusted_keys = install_policy(home.path());
    fs::write(
        home.path().join("sprint.jsonl"),
        concat!(
            r#"{"ts":"2026-08-22T06:00:00Z","kind":"work-completed","fact":{"item_id":"placeholder-org/alpha#337","order_id":"placeholder-order-337","unit_name":"ostrom-implementer-placeholder","pr_url":"https://github.com/placeholder-org/alpha/pull/372"},"narration":{}}"#,
            "\n",
        ),
    )
    .expect("write synthetic completion trace");
    let fixture = home.path().join("fixture.json");
    fs::write(
        &fixture,
        serde_json::to_vec(&json!({
            "repositories": [{
                "repo": REPOSITORY,
                "issues": [issue(337), issue(422), issue(999)],
                "open_prs": [],
                "merged_prs": [
                    {
                        "number": 407,
                        "title": "Re-land failed implementation",
                        "body": "Closes placeholder-org/alpha#372",
                        "author": {"login": "placeholder-builder", "isBot": false},
                        "closingIssuesReferences": [],
                        "createdAt": "2026-08-22T06:30:00Z",
                        "mergedAt": "2026-08-22T07:13:51Z",
                        "headRefOid": "placeholder-407-head",
                        "headRefName": "ostrom/372-f4d7286e4868",
                        "state": "MERGED"
                    },
                    {
                        "number": 425,
                        "title": "Authority resolves by rule kind",
                        "body": "Implements #422, the settled design",
                        "author": {"login": "placeholder-human", "isBot": false},
                        "closingIssuesReferences": [],
                        "createdAt": "2026-08-23T07:00:00Z",
                        "mergedAt": "2026-08-23T07:59:12Z",
                        "headRefOid": "placeholder-425-head",
                        "headRefName": "ostrom/422-921c0ce312c0",
                        "state": "MERGED"
                    }
                ],
                "default_branch": "main",
                "branches": [],
                "branch_read_degraded": false,
                "ci_runs": []
            }]
        }))
        .expect("serialize synthetic acquisition"),
    )
    .expect("write synthetic acquisition");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "sweep",
            "--fixture",
            fixture.to_str().expect("UTF-8 fixture path"),
            "--started-at",
            "2026-08-24T00:00:00Z",
        ])
        .env("OSTROM_HOME", home.path())
        .env("OSTROM_POLICY_TRUSTED_KEYS", &trusted_keys)
        .current_dir(home.path())
        .output()
        .expect("run item-closure sweep");
    assert!(
        output.status.success(),
        "sweep stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let rows = queue(home.path());
    let closure_fault = |item: u64| {
        rows.iter()
            .find(|row| row["id"] == format!("{REPOSITORY}#{item}"))
            .unwrap_or_else(|| panic!("missing closure fault for item {item}: {rows:#?}"))
    };
    let route_one = closure_fault(337);
    assert_eq!(route_one["kind"], "merge-gate-fault");
    assert!(
        route_one["mandate"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("placeholder-org/alpha#337")
                && reason.contains("placeholder-org/alpha#407"))
    );
    assert_eq!(
        route_one["mandate"]["scope_evidence"]["branch_item_id"],
        "placeholder-org/alpha#372"
    );
    assert_eq!(
        route_one["mandate"]["scope_evidence"]["carried_forward"],
        true
    );

    let route_two = closure_fault(422);
    assert!(
        route_two["mandate"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("placeholder-org/alpha#422")
                && reason.contains("placeholder-org/alpha#425"))
    );
    assert_eq!(
        route_two["mandate"]["scope_evidence"]["carried_forward"],
        false
    );
    assert!(
        rows.iter()
            .all(|row| row["id"] != "placeholder-org/alpha#999"),
        "an item with no merged implementation must not be faulted: {rows:#?}"
    );

    let state: Value = serde_json::from_slice(
        &fs::read(home.path().join("state.json")).expect("read sweep state"),
    )
    .expect("parse sweep state");
    assert_eq!(state["repos"][REPOSITORY]["item_closure_fault_count"], 2);
    assert_eq!(
        state["repos"][REPOSITORY]["item_closure_faults"]
            .as_object()
            .map(serde_json::Map::len),
        Some(2)
    );
}
