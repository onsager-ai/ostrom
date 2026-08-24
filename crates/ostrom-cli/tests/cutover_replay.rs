use std::{fs, process::Command};

use tempfile::tempdir;

#[test]
fn cutover_replay_prints_three_pasteable_empty_diffs_from_one_snapshot() {
    let root = tempdir().expect("cutover fixture");
    let legacy = root.path().join("legacy");
    let scratch = root.path().join("scratch");
    fs::create_dir_all(legacy.join("systemd")).expect("legacy systemd");
    fs::create_dir(&scratch).expect("scratch home");
    fs::write(
        legacy.join("mandates.yaml"),
        r#"provider: file
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
    default: delegated
    paused: false
    bounce: []
"#,
    )
    .expect("legacy mandates");
    fs::write(
        legacy.join("gate.yaml"),
        r#"provider: file
bounce_all: []
projects:
  - repo: placeholder-org/alpha
    required_checks: [verify-linux]
    bounce: []
    reserved: []
"#,
    )
    .expect("legacy gate");
    fs::write(
        legacy.join("systemd/enabled-timers"),
        "ostrom-loop-builder.timer\n",
    )
    .expect("enabled timers");
    fs::write(
        legacy.join("systemd/ostrom-loop-builder.timer"),
        "[Timer]\nOnCalendar=hourly\n",
    )
    .expect("legacy timer");
    fs::write(
        legacy.join("systemd/ostrom-loop-builder.service"),
        "[Service]\nEnvironment=OSTROM_ACTOR=builder\nEnvironment=MANDATE_MAX_IMPLEMENTERS=6\nEnvironment=MANDATE_DAILY_CAP_USD=50\nEnvironment=MANDATE_ORDER_TOKEN_CEILING=200000\n",
    )
    .expect("legacy service");

    let manifest = root.path().join("ostrom.yaml");
    fs::write(
        &manifest,
        r#"manifest_version: 1
defaults:
  loop: {concurrent: 6, spend_usd: 50, tokens: 200000}
actors: {builder: {}, gatekeeper: {}}
checks:
  verify-linux:
    uses: gh/check-run
    with: {name: verify-linux}
operations: {work: {steps: []}, merge: {steps: []}}
grants:
  delegated-fixes: {actors: builder, operations: work, repositories: placeholder-org/alpha, where: type:fix}
  gatekeeper-merge: {actors: gatekeeper, operations: merge, repositories: placeholder-org/alpha, requires: verify-linux}
loops:
  builder: {actor: builder, operation: work, target: placeholder-org/alpha, every: hourly}
"#,
    )
    .expect("manifest");
    let parsed = ostrom_core::PolicyManifest::parse_yaml(
        &fs::read_to_string(&manifest).expect("read manifest"),
    )
    .expect("parse manifest");
    let digest = ostrom_store::policy_manifest_digest(&parsed).expect("manifest digest");
    let version = root.path().join("versions").join(digest);
    fs::create_dir_all(&version).expect("version directory");
    fs::write(
        version.join("ostrom.yaml"),
        parsed.to_yaml().expect("canonical manifest"),
    )
    .expect("materialized manifest");

    let snapshot = root.path().join("snapshot.json");
    fs::write(
        &snapshot,
        serde_json::to_vec_pretty(&serde_json::json!({
            "repositories": [{
                "repo": "placeholder-org/alpha",
                "issues": [],
                "open_prs": [{
                    "number": 12,
                    "title": "fix: placeholder",
                    "body": "",
                    "author": {"login": "placeholder-author"},
                    "headRefOid": "aaaaaaaa",
                    "labels": [],
                    "closingIssuesReferences": [],
                    "mergeable": "MERGEABLE",
                    "isDraft": false,
                    "files": [{"path": "src/lib.rs"}],
                    "checks": [{"name": "verify-linux", "conclusion": "SUCCESS"}],
                    "createdAt": "2026-08-20T00:00:00Z",
                    "updatedAt": "2026-08-20T00:00:00Z",
                    "state": "OPEN"
                }],
                "merged_prs": [],
                "branches": [],
                "branch_read_degraded": false,
                "ci_runs": [],
                "warnings": []
            }],
            "gates": {"placeholder-org/alpha#12": {
                "metadata_ready": true,
                "metadata": {
                    "number": 12,
                    "title": "fix: placeholder",
                    "author": {"login": "placeholder-author"},
                    "headRefOid": "aaaaaaaa",
                    "labels": [],
                    "closingIssuesReferences": [],
                    "mergeable": "MERGEABLE",
                    "isDraft": false,
                    "files": [{"path": "src/lib.rs"}],
                    "checks": [{"name": "verify-linux", "conclusion": "SUCCESS"}]
                },
                "metadata_error": "",
                "head_sha": "aaaaaaaa",
                "checks_ready": true,
                "checks": [{"name": "verify-linux", "conclusion": "SUCCESS"}],
                "checks_error": "",
                "checks_partial_error": "",
                "diff_ready": true,
                "paths": ["src/lib.rs"],
                "diff_error": "",
                "diff_content_ready": false,
                "diff_content": "",
                "diff_content_error": "diff content was not requested",
                "threads_ready": true,
                "threads": [],
                "threads_error": "",
                "thread_author": "placeholder-author"
            }}
        }))
        .expect("snapshot JSON"),
    )
    .expect("snapshot");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["parity", "cutover", "--legacy"])
        .arg(&legacy)
        .arg("--manifest")
        .arg(&version)
        .arg("--snapshot")
        .arg(&snapshot)
        .args(["--started-at", "2026-08-24T00:00:00Z"])
        .env("OSTROM_HOME", &scratch)
        .output()
        .expect("run cutover replay");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 evidence"),
        "gate 1 classification: repositories=1 items=1\n  covered: placeholder-org/alpha\n  diff: empty\n\
gate 2 gate verdict: repositories=1 items=1\n  covered: placeholder-org/alpha\n  diff: empty\n\
gate 3 loop equivalence: repositories=1 items=1\n  covered: placeholder-org/alpha\n  diff: empty\n"
    );
    assert!(
        fs::read_dir(&scratch)
            .expect("read scratch")
            .next()
            .is_none(),
        "temporary replay state must be removed"
    );
}
