use std::{fs, path::Path, process::Command};

use tempfile::tempdir;

const STARTED_AT: &str = "2026-08-01T00:00:00Z";

fn corpus(path: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/parity-sweep")
        .join(path)
}

fn scratch_home(root: &Path) -> std::path::PathBuf {
    let home = root.join("scratch-home");
    fs::create_dir(&home).expect("create scratch OSTROM_HOME");
    fs::copy(corpus("mandates.yaml"), home.join("mandates.yaml"))
        .expect("copy recorded placeholder roster");
    home
}

fn run_parity(home: &Path, recorded_queue: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["parity", "sweep", "--started-at", STARTED_AT, "--fixture"])
        .arg(corpus("github.json"))
        .arg("--recorded-queue")
        .arg(recorded_queue)
        .env("OSTROM_HOME", home)
        .env("MANDATE_PUBLISH_REMOTE", "placeholder-org/forbidden-target")
        .current_dir(home)
        .output()
        .expect("run recorded parity sweep")
}

#[test]
fn recorded_parity_is_keyed_by_id_reports_fields_and_cannot_publish() {
    let root = tempdir().expect("temporary parity fixture");
    let home = scratch_home(root.path());

    let equal = run_parity(&home, &corpus("queue.shell.jsonl"));
    assert!(
        equal.status.success(),
        "parity stderr: {}",
        String::from_utf8_lossy(&equal.stderr)
    );
    assert!(String::from_utf8_lossy(&equal.stdout).contains("zero divergences across 1 row(s)"));
    assert!(!home.join("queue.jsonl").exists());
    assert!(!home.join("state.json").exists());
    assert!(!home.join("publish").exists());

    let mut row: serde_json::Value = serde_json::from_str(
        fs::read_to_string(corpus("queue.shell.jsonl"))
            .expect("read recorded shell queue")
            .trim(),
    )
    .expect("parse recorded shell row");
    row["mandate"]["reason"] = serde_json::json!("seeded placeholder divergence");
    let divergent_queue = root.path().join("queue.divergent.jsonl");
    fs::write(
        &divergent_queue,
        format!(
            "{}\n",
            serde_json::to_string(&row).expect("encode changed row")
        ),
    )
    .expect("seed field difference");

    let different = run_parity(&home, &divergent_queue);
    assert_eq!(different.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&different.stdout);
    assert!(stdout.contains("mandate.reason differs on 1 row(s)"));
    assert!(stdout.contains("placeholder-org/alpha@refs/heads/ostrom/unmatched"));
    assert!(!home.join("publish").exists());
}

#[test]
fn parity_names_a_missing_recorded_queue() {
    let root = tempdir().expect("temporary missing-recording fixture");
    let home = scratch_home(root.path());
    let missing = root.path().join("missing-shell-queue.jsonl");
    let output = run_parity(&home, &missing);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("parity sweep recorded queue is missing"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
