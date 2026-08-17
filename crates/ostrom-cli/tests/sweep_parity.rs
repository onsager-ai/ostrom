use std::{fs, path::Path, process::Command};

use tempfile::tempdir;

const ROSTER: &str = r#"
provider: file
cadence_hours: 1
stuck_after_days: 7
search_roots: []
hold_labels: []
bounce_all:
  - title:*credential*
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

#[test]
fn fixture_sweep_queue_is_byte_identical_to_recorded_cross_org_output() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let home = tempdir().expect("temporary OSTROM_HOME");
    fs::write(home.path().join("mandates.yaml"), ROSTER).expect("write fixture roster");
    fs::write(
        home.path().join("gate.jsonl"),
        concat!(
            r#"{"ts":"2026-07-10T00:00:00Z","pr":"example-org/example-repo#1","head_sha":"0000000000000000000000000000000000000000","verdict":"pass"}"#,
            "\n",
        ),
    )
    .expect("write synthetic gate evidence");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "sweep",
            "--fixture",
            root.join("tests/fixtures/sweep-cross-org.json")
                .to_str()
                .expect("fixture path is UTF-8"),
            "--started-at",
            "2026-08-01T00:00:00Z",
        ])
        .env("OSTROM_HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("run Rust fixture sweep");
    assert!(
        output.status.success(),
        "sweep stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let actual = fs::read_to_string(home.path().join("queue.jsonl")).expect("read generated queue");
    let expected = fs::read_to_string(root.join("tests/fixtures/sweep-cross-org.expected.jsonl"))
        .expect("read expected queue");
    assert_eq!(actual, expected, "fixture queue parity invariant failed");
}

#[test]
fn fixture_sweep_turns_a_stale_work_ranking_pointer_into_a_visible_fault() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let home = tempdir().expect("temporary OSTROM_HOME");
    let ranked_roster = ROSTER.replace(
        "hold_labels: []\n",
        concat!(
            "hold_labels: []\n",
            "work_ranking:\n",
            "  - example-org/example-repo#999\n",
        ),
    );
    fs::write(home.path().join("mandates.yaml"), ranked_roster)
        .expect("write ranked fixture roster");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "sweep",
            "--fixture",
            root.join("tests/fixtures/sweep-cross-org.json")
                .to_str()
                .expect("fixture path is UTF-8"),
            "--started-at",
            "2026-08-01T00:00:00Z",
        ])
        .env("OSTROM_HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("run ranked fixture sweep");
    assert!(
        output.status.success(),
        "sweep stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("recorded work_ranking item no longer exists: example-org/example-repo#999"),
        "stale ranking was not reported: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let queue = fs::read_to_string(home.path().join("queue.jsonl")).expect("read fault queue");
    let fault = queue
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse queue row"))
        .find(|row| row["id"] == "example-org/example-repo#999")
        .expect("stale ranking fault row");
    assert_eq!(fault["kind"], "drift");
    assert_eq!(
        fault["mandate"]["reason"],
        "work_ranking item no longer exists: example-org/example-repo#999"
    );
    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(home.path().join("state.json")).expect("read ranked state"),
    )
    .expect("parse ranked state");
    assert_eq!(
        state["work_ranking"],
        serde_json::json!(["example-org/example-repo#999"])
    );
    assert_eq!(
        state["work_ranking_faults"],
        serde_json::json!(["example-org/example-repo#999"])
    );
}

#[test]
fn fixture_sweep_refuses_a_query_at_the_exhaustiveness_cap() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let home = tempdir().expect("temporary OSTROM_HOME");
    fs::write(home.path().join("mandates.yaml"), ROSTER).expect("write fixture roster");
    let fixture: serde_json::Value = serde_json::from_slice(
        &fs::read(root.join("tests/fixtures/sweep-cross-org.json")).expect("read fixture"),
    )
    .expect("parse fixture");
    let mut fixture = fixture;
    let repeated = fixture["repositories"][0]["issues"][0].clone();
    fixture["repositories"][0]["issues"] = serde_json::Value::Array(vec![repeated; 200]);
    let truncated = home.path().join("truncated.json");
    fs::write(
        &truncated,
        serde_json::to_vec(&fixture).expect("serialize truncated fixture"),
    )
    .expect("write truncated fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "sweep",
            "--fixture",
            truncated.to_str().expect("fixture path is UTF-8"),
            "--started-at",
            "2026-08-01T00:00:00Z",
        ])
        .env("OSTROM_HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("run truncated fixture sweep");
    assert!(!output.status.success(), "truncated sweep unexpectedly ran");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("refusing a truncated sweep"),
        "stderr did not name truncation: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !home.path().join("queue.jsonl").exists(),
        "truncated fixture wrote a partial queue"
    );
}

#[test]
fn incremental_fixture_retains_unchanged_issue_records_and_queue_bytes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let home = tempdir().expect("temporary OSTROM_HOME");
    fs::write(home.path().join("mandates.yaml"), ROSTER).expect("write fixture roster");
    fs::write(
        home.path().join("gate.jsonl"),
        concat!(
            r#"{"ts":"2026-07-10T00:00:00Z","pr":"example-org/example-repo#1","head_sha":"0000000000000000000000000000000000000000","verdict":"pass"}"#,
            "\n",
        ),
    )
    .expect("write synthetic gate evidence");
    let fixture_path = root.join("tests/fixtures/sweep-cross-org.json");
    let first = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "sweep",
            "--fixture",
            fixture_path.to_str().expect("fixture path is UTF-8"),
            "--started-at",
            "2026-08-01T00:00:00Z",
        ])
        .env("OSTROM_HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("run full fixture sweep");
    assert!(first.status.success());
    let before = fs::read(home.path().join("queue.jsonl")).expect("read baseline queue");

    let mut fixture: serde_json::Value =
        serde_json::from_slice(&fs::read(&fixture_path).expect("read fixture"))
            .expect("parse fixture");
    fixture["repositories"][0]["issues"] = serde_json::json!([]);
    fixture["repositories"][0]["issue_not_modified"] = serde_json::json!(true);
    let incremental_fixture = home.path().join("incremental.json");
    fs::write(
        &incremental_fixture,
        serde_json::to_vec(&fixture).expect("serialize incremental fixture"),
    )
    .expect("write incremental fixture");
    let second = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "sweep",
            "--fixture",
            incremental_fixture.to_str().expect("fixture path is UTF-8"),
            "--mode",
            "incremental",
            "--started-at",
            "2026-08-01T01:00:00Z",
        ])
        .env("OSTROM_HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("run incremental fixture sweep");
    assert!(
        second.status.success(),
        "incremental stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        before,
        fs::read(home.path().join("queue.jsonl")).expect("read incremental queue")
    );
    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(home.path().join("state.json")).expect("read incremental state"),
    )
    .expect("parse incremental state");
    assert_eq!(state["sweep_mode"], "incremental");
    assert!(
        state["repos"]["example-org/example-repo"]["records"]
            .get("example-org/example-repo#10")
            .is_some(),
        "304-style empty delta dropped the retained issue record"
    );
}

#[cfg(unix)]
#[test]
fn real_driver_authenticates_once_per_organization_and_faults_failed_orgs() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempdir().expect("temporary OSTROM_HOME");
    let plugin = home.path().join("plugin");
    fs::create_dir_all(plugin.join("scripts")).expect("create fake plugin");
    fs::write(home.path().join("mandates.yaml"), ROSTER).expect("write fixture roster");
    let auth_log = home.path().join("auth.log");
    let fake_auth = plugin.join("scripts/gh-as.sh");
    fs::write(
        &fake_auth,
        r#"#!/usr/bin/env bash
set -eu
printf '%s\n' "$2" >> "$OSTROM_TEST_AUTH_LOG"
case "$2" in
  another-example-org/another-example-repo)
    printf '%s\n' '{"repositories":[{"repo":"another-example-org/another-example-repo","issues":[],"open_prs":[],"merged_prs":[],"ci_runs":[]}]}'
    ;;
  example-org/example-repo)
    printf '%s\n' 'synthetic authentication failure' >&2
    exit 111
    ;;
esac
"#,
    )
    .expect("write fake authentication wrapper");
    fs::set_permissions(&fake_auth, fs::Permissions::from_mode(0o700))
        .expect("make fake authentication wrapper executable");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["sweep", "--started-at", "2026-08-01T00:00:00Z"])
        .env("OSTROM_HOME", home.path())
        .env("OSTROM_PLUGIN_ROOT", &plugin)
        .env("OSTROM_TEST_AUTH_LOG", &auth_log)
        .current_dir(home.path())
        .output()
        .expect("run organization driver");
    assert!(
        output.status.success(),
        "driver stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let anchors = fs::read_to_string(auth_log).expect("read authentication log");
    assert_eq!(
        anchors,
        concat!(
            "another-example-org/another-example-repo\n",
            "example-org/example-repo\n"
        ),
        "driver did not mint exactly one token per organization"
    );
    let rows = fs::read_to_string(home.path().join("queue.jsonl")).expect("read fault queue");
    let rows = rows
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse queue row"))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], "example-org/example-repo#0");
    assert!(
        rows[0]["mandate"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("authentication")),
        "authentication failure was not a named queue fault"
    );
}

#[cfg(unix)]
#[test]
fn github_worker_uses_incremental_issue_feed_and_bounded_recent_pr_queries() {
    use std::{env, os::unix::fs::PermissionsExt};

    let home = tempdir().expect("temporary OSTROM_HOME");
    let bin = home.path().join("bin");
    fs::create_dir(&bin).expect("create fake binary directory");
    fs::write(
        home.path().join("mandates.yaml"),
        ROSTER.replace(
            concat!(
                "  - repo: another-example-org/another-example-repo\n",
                "    delegated: [type:fix]\n",
                "    excluded: []\n",
                "    reserved: []\n",
                "    default: excluded\n",
                "    paused: false\n",
                "    bounce: []\n"
            ),
            "",
        ),
    )
    .expect("write one-organization roster");
    fs::write(
        home.path().join("state.json"),
        r#"{"version":2,"repos":{"example-org/example-repo":{"cursor":"2026-07-31T00:00:00Z","etag":"fixture-etag","records":{}}}}"#,
    )
    .expect("write incremental state");
    let log = home.path().join("gh.log");
    let fake_gh = bin.join("gh");
    fs::write(
        &fake_gh,
        r#"#!/usr/bin/env bash
set -eu
printf '%s\n' "$*" >> "$OSTROM_TEST_GH_LOG"
case "$1 $2" in
  "auth status") exit 0 ;;
  "api -X")
    printf 'HTTP/2 304 Not Modified\r\netag: fixture-etag\r\n\r\n'
    exit 1
    ;;
  "api graphql")
    printf '%s\n' '{"data":{"repository":{"issues":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}'
    ;;
  "pr list") printf '%s\n' '[]' ;;
  "repo view") printf '%s\n' '{"defaultBranchRef":{"name":"main"}}' ;;
  "run list") printf '%s\n' '[]' ;;
  *) exit 9 ;;
esac
"#,
    )
    .expect("write fake gh");
    fs::set_permissions(&fake_gh, fs::Permissions::from_mode(0o700))
        .expect("make fake gh executable");
    let path = format!(
        "{}:{}",
        bin.display(),
        env::var("PATH").expect("PATH is configured")
    );

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "sweep",
            "--inner-org",
            "example-org",
            "--mode",
            "incremental",
            "--started-at",
            "2026-08-01T00:00:00Z",
        ])
        .env("OSTROM_HOME", home.path())
        .env("OSTROM_TEST_GH_LOG", &log)
        .env("PATH", path)
        .current_dir(home.path())
        .output()
        .expect("run GitHub worker");
    assert!(
        output.status.success(),
        "worker stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshots: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse worker snapshots");
    assert_eq!(snapshots["repositories"][0]["issue_not_modified"], true);
    let calls = fs::read_to_string(log).expect("read gh call log");
    assert!(calls.contains("If-None-Match: fixture-etag"));
    assert!(calls.contains("since=2026-07-31T00:00:00Z"));
    assert!(calls.contains("api graphql -f query=query OstromDependencyGraph"));
    assert!(calls.contains("pr list --repo example-org/example-repo --state open --limit 200"));
    assert!(calls.contains(
        "pr list --repo example-org/example-repo --state merged --search merged:>=2026-07-02 --limit 200"
    ));
    assert!(!calls.contains("--state all --limit 200"));
    assert!(calls.contains("run list --repo example-org/example-repo --branch main --limit 200"));
}
