use std::{fs, path::Path, process::Command};

use tempfile::TempDir;

mod support;

const STARTED_AT: &str = "2026-08-23T00:00:00Z";

fn fixture() -> TempDir {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/roster-coverage");
    support::copy_fixture_directory(&source)
}

fn run_sweep(home: &Path, trusted_keys: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "sweep",
            "--mode",
            "full",
            "--started-at",
            STARTED_AT,
            "--fixture",
        ])
        .arg(home.join("github.json"))
        .env("OSTROM_HOME", home)
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted_keys)
        .current_dir(home)
        .output()
        .expect("run roster coverage sweep")
}

#[test]
fn delegated_without_gate_fixture_emits_exactly_one_named_finding() {
    let root = fixture();
    let trusted_keys = support::sign_manifest(&root.path().join("ostrom.yaml"));
    let output = run_sweep(root.path(), &trusted_keys);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(root.path().join("state.json")).expect("read sweep state"),
    )
    .expect("parse sweep state");
    assert_eq!(
        state["roster_coverage"],
        serde_json::json!([{
            "repo": "placeholder-org/delegated-repository",
            "finding": "delegated_without_merge_gate",
            "missing_document": "gate.yaml",
        }])
    );
}

#[test]
fn repository_present_in_both_documents_has_no_finding() {
    let root = fixture();
    fs::write(
        root.path().join("gate.yaml"),
        concat!(
            "provider: file\n",
            "projects:\n",
            "  - repo: placeholder-org/delegated-repository\n",
            "    required_checks: []\n",
            "    bounce: []\n",
            "    reserved: []\n",
        ),
    )
    .expect("write matching gate policy");
    let trusted_keys = support::sign_manifest(&root.path().join("ostrom.yaml"));
    let output = run_sweep(root.path(), &trusted_keys);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(root.path().join("state.json")).expect("read sweep state"),
    )
    .expect("parse sweep state");
    assert_eq!(state["roster_coverage"], serde_json::json!([]));
}

#[test]
fn validate_refuses_delegated_without_gate_but_accepts_an_ungoverned_repository() {
    let governed = fixture();
    let trusted_keys = support::sign_manifest(&governed.path().join("ostrom.yaml"));
    let invalid = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["validate"])
        .arg(governed.path().join("ostrom.yaml"))
        .env("OSTROM_POLICY_TRUSTED_KEYS", &trusted_keys)
        .current_dir(governed.path())
        .output()
        .expect("validate mismatched legacy policy");
    assert!(!invalid.status.success());
    let stderr = String::from_utf8_lossy(&invalid.stderr);
    assert!(
        stderr.contains("placeholder-org/delegated-repository"),
        "{stderr}"
    );
    assert!(stderr.contains("gate.yaml"), "{stderr}");

    let ungoverned = tempfile::tempdir().expect("ungoverned repository fixture");
    fs::write(
        ungoverned.path().join("ostrom.yaml"),
        "manifest_version: 1\n",
    )
    .expect("write ungoverned manifest");
    let trusted_keys = support::sign_manifest(&ungoverned.path().join("ostrom.yaml"));
    let valid = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["validate"])
        .arg(ungoverned.path().join("ostrom.yaml"))
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted_keys)
        .current_dir(ungoverned.path())
        .output()
        .expect("validate ungoverned repository");
    assert!(
        valid.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&valid.stderr)
    );
}
