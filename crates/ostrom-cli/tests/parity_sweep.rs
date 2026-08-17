use std::{fs, path::Path, process::Command};

use tempfile::tempdir;

const STARTED_AT: &str = "2026-08-01T00:00:00Z";
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

fn write_fixture(root: &Path) -> std::path::PathBuf {
    let fixture = root.join("fixture.json");
    fs::write(
        &fixture,
        r#"{"repositories":[{"repo":"placeholder-org/alpha","issues":[],"open_prs":[],"merged_prs":[],"default_branch":"main","branches":[{"name":"ostrom/unmatched","commit":{"sha":"placeholder-sha"}}],"branch_read_degraded":false,"ci_runs":[]}]}"#,
    )
    .expect("write placeholder fixture");
    fixture
}

fn baseline_queue(root: &Path, fixture: &Path) -> String {
    let home = root.join("baseline-home");
    fs::create_dir(&home).expect("create baseline home");
    fs::write(home.join("mandates.yaml"), ROSTER).expect("write baseline roster");
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["sweep", "--started-at", STARTED_AT, "--fixture"])
        .arg(fixture)
        .env("OSTROM_HOME", &home)
        .current_dir(&home)
        .output()
        .expect("run baseline native sweep");
    assert!(
        output.status.success(),
        "baseline stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::read_to_string(home.join("queue.jsonl")).expect("read baseline queue")
}

fn write_fake_plugin(root: &Path) -> std::path::PathBuf {
    let plugin = root.join("plugin");
    let scripts = plugin.join("scripts");
    fs::create_dir_all(&scripts).expect("create fake scripts");
    fs::write(
        scripts.join("sweep.sh"),
        r#"#!/usr/bin/env bash
set -eu
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
printf '%s\n' "$MANDATE_SWEEP_TIME" > "$PARITY_CLOCK_LOG"
set +e
bash "$SCRIPT_DIR/publish.sh"
publish_status=$?
set -e
[ "$publish_status" -eq 3 ] || {
  printf 'publication edge was not disabled\n' >&2
  exit 9
}
data="$CLAUDE_CONFIG_DIR/ostrom"
cp "$data/shell-queue.jsonl" "$data/queue.jsonl"
printf '%s\n' '{}' > "$data/state.json"
"#,
    )
    .expect("write fake sweep");
    fs::write(
        scripts.join("publish.sh"),
        r#"#!/usr/bin/env bash
printf 'published\n' > "$PARITY_PUBLISH_MARKER"
exit 0
"#,
    )
    .expect("write hostile fake publisher");
    plugin
}

fn run_parity(
    scratch_home: &Path,
    plugin: &Path,
    fixture: &Path,
    clock_log: &Path,
    publish_marker: &Path,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["parity", "sweep", "--started-at", STARTED_AT, "--fixture"])
        .arg(fixture)
        .env("OSTROM_HOME", scratch_home)
        .env("OSTROM_PLUGIN_ROOT", plugin)
        .env("PARITY_CLOCK_LOG", clock_log)
        .env("PARITY_PUBLISH_MARKER", publish_marker)
        .env("MANDATE_PUBLISH_REMOTE", "placeholder-org/forbidden-target")
        .current_dir(scratch_home)
        .output()
        .expect("run parity sweep")
}

#[test]
fn parity_is_keyed_by_id_reports_fields_and_cannot_publish() {
    let root = tempdir().expect("temporary parity fixture");
    let fixture = write_fixture(root.path());
    let expected = baseline_queue(root.path(), &fixture);
    let scratch_home = root.path().join("scratch-home");
    fs::create_dir(&scratch_home).expect("create scratch OSTROM_HOME");
    fs::write(scratch_home.join("mandates.yaml"), ROSTER).expect("write scratch roster");
    fs::write(scratch_home.join("shell-queue.jsonl"), &expected).expect("write shell baseline");
    let plugin = write_fake_plugin(root.path());
    let clock_log = root.path().join("clock.log");
    let publish_marker = root.path().join("published.marker");

    let equal = run_parity(
        &scratch_home,
        &plugin,
        &fixture,
        &clock_log,
        &publish_marker,
    );
    assert!(
        equal.status.success(),
        "parity stderr: {}",
        String::from_utf8_lossy(&equal.stderr)
    );
    assert!(String::from_utf8_lossy(&equal.stdout).contains("zero divergences across 1 row(s)"));
    assert_eq!(
        fs::read_to_string(&clock_log).expect("read shell clock"),
        format!("{STARTED_AT}\n")
    );
    assert!(!publish_marker.exists(), "parity reached the publisher");
    assert!(!scratch_home.join("queue.jsonl").exists());
    assert!(!scratch_home.join("state.json").exists());

    let mut row: serde_json::Value =
        serde_json::from_str(expected.trim()).expect("parse baseline row");
    row["mandate"]["reason"] = serde_json::json!("seeded placeholder divergence");
    fs::write(
        scratch_home.join("shell-queue.jsonl"),
        format!(
            "{}\n",
            serde_json::to_string(&row).expect("encode changed row")
        ),
    )
    .expect("seed field difference");
    let different = run_parity(
        &scratch_home,
        &plugin,
        &fixture,
        &clock_log,
        &publish_marker,
    );
    assert_eq!(different.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&different.stdout);
    assert!(stdout.contains("mandate.reason differs on 1 row(s)"));
    assert!(stdout.contains("placeholder-org/alpha@refs/heads/ostrom/unmatched"));
    assert!(
        !publish_marker.exists(),
        "divergent parity reached the publisher"
    );
}

#[test]
fn parity_names_a_missing_legacy_target() {
    let root = tempdir().expect("temporary missing-script fixture");
    let scratch_home = root.path().join("scratch-home");
    let plugin = root.path().join("plugin");
    fs::create_dir_all(&scratch_home).expect("create scratch home");
    fs::create_dir_all(&plugin).expect("create empty plugin");
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["parity", "sweep", "--started-at", STARTED_AT])
        .env("OSTROM_HOME", &scratch_home)
        .env("OSTROM_PLUGIN_ROOT", &plugin)
        .current_dir(&scratch_home)
        .output()
        .expect("run parity without legacy sweep");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("parity sweep comparison target is missing"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
