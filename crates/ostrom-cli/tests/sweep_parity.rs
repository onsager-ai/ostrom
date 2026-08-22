use std::{fs, path::Path, process::Command, time::SystemTime};

use chrono::{DateTime, Utc};
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
    case "$*" in
      *"/branches?"*) printf '%s\n' '[]'; exit 0 ;;
    esac
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
    assert!(calls.contains(
        "number,title,author,mergedBy,closingIssuesReferences,createdAt,mergedAt,headRefName,headRefOid,state"
    ));
    assert!(!calls.contains("--state all --limit 200"));
    assert!(
        calls.contains("api -X GET repos/example-org/example-repo/branches?per_page=100&page=1")
    );
    assert!(calls.contains("run list --repo example-org/example-repo --branch main --limit 200"));
}

const PLACEHOLDER_ROSTER: &str = r#"
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

fn write_placeholder_fixture(home: &Path, repository: serde_json::Value) -> std::path::PathBuf {
    fs::write(home.join("mandates.yaml"), PLACEHOLDER_ROSTER).expect("write placeholder roster");
    let fixture = home.join("fixture.json");
    fs::write(
        &fixture,
        serde_json::to_vec(&serde_json::json!({"repositories": [repository]}))
            .expect("serialize placeholder fixture"),
    )
    .expect("write placeholder fixture");
    fixture
}

fn run_placeholder_sweep(home: &Path, fixture: &Path, extra: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
    command.args(["sweep", "--fixture"]);
    command.arg(fixture);
    command.args(extra);
    command
        .env("OSTROM_HOME", home)
        .current_dir(home)
        .output()
        .expect("run placeholder sweep")
}

fn placeholder_repository() -> serde_json::Value {
    serde_json::json!({
        "repo": "placeholder-org/alpha",
        "issues": [],
        "open_prs": [],
        "merged_prs": [],
        "default_branch": "main",
        "branches": [],
        "branch_read_degraded": false,
        "ci_runs": [],
    })
}

#[test]
fn unexplained_merges_and_reserved_branches_share_the_alarm_kind() {
    let home = tempdir().expect("temporary OSTROM_HOME");
    let mut repository = placeholder_repository();
    repository["default_branch"] = serde_json::json!("ostrom/default");
    repository["branches"] = serde_json::json!([
        {"name": "ostrom/default", "commit": {"sha": "default-sha"}},
        {"name": "ostrom/unmatched", "commit": {"sha": "branch-sha"}},
        {"name": "ostrom/matched", "commit": {"sha": "matched-sha"}},
        {"name": "feature/outside", "commit": {"sha": "outside-sha"}}
    ]);
    repository["merged_prs"] = serde_json::json!([
        {
            "number": 1,
            "title": "Machine merge without order",
            "author": {"login": "builder[bot]", "is_bot": true},
            "closingIssuesReferences": [],
            "createdAt": "2026-07-04T00:00:00Z",
            "mergedAt": "2026-07-05T00:00:00Z",
            "headRefOid": "machine-unexplained-sha"
        },
        {
            "number": 2,
            "title": "Machine merge with order",
            "author": {"login": "builder[bot]", "is_bot": true},
            "closingIssuesReferences": [{"number": 99}],
            "createdAt": "2026-07-04T00:00:00Z",
            "mergedAt": "2026-07-05T00:00:00Z",
            "headRefOid": "machine-explained-sha"
        },
        {
            "number": 3,
            "title": "Human merge with order",
            "author": {"login": "placeholder-human", "is_bot": false},
            "closingIssuesReferences": [{"number": 99}],
            "createdAt": "2026-07-04T00:00:00Z",
            "mergedAt": "2026-07-05T00:00:00Z",
            "headRefOid": "human-explained-sha"
        }
    ]);
    let fixture = write_placeholder_fixture(home.path(), repository);
    fs::write(
        home.path().join("gate.jsonl"),
        concat!(
            r#"{"ts":"2026-07-01T00:00:00Z","pr":"placeholder-org/alpha#90","head_sha":"floor-sha","verdict":"pass"}"#,
            "\n",
        ),
    )
    .expect("write placeholder gate floor");
    fs::create_dir(home.path().join("work-orders")).expect("create work-order directory");
    fs::write(
        home.path().join("work-orders/matched.json"),
        r#"{"repository":"placeholder-org/alpha","branch_name":"ostrom/matched","item_id":"placeholder-org/alpha#99","order_id":"placeholder-order"}"#,
    )
    .expect("write matching work order");
    fs::write(
        home.path().join("state.json"),
        r#"{"version":2,"repos":{"placeholder-org/alpha":{"merge_gate_faults":{"placeholder-org/alpha#1":{"fingerprint":"scope-v1|no_verdict|machine-unexplained-sha|none||true|"}}}}}"#,
    )
    .expect("write prior merge fingerprint");
    fs::write(
        home.path().join("queue.jsonl"),
        concat!(
            r##"{"id":"placeholder-org/alpha#1","repo":"placeholder-org/alpha","ref":"#1","title":"Machine merge without order","kind":"merge-gate-fault","mandate":{"reason":"merge gate fault: no verdict for merged head machine-unexplained-sha"},"state":"pending","opened":"2026-07-05T00:00:00Z","age_days":0,"aged_out":false,"needs_judgment":false,"blocked_by":[]}"##,
            "\n",
        ),
    )
    .expect("write prior queue kind");

    let output = run_placeholder_sweep(
        home.path(),
        &fixture,
        &["--started-at", "2026-08-01T00:00:00Z"],
    );
    assert!(
        output.status.success(),
        "sweep stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = fs::read_to_string(home.path().join("queue.jsonl")).expect("read queue");
    let rows = rows
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse queue row"))
        .collect::<Vec<_>>();
    let row = |id: &str| {
        rows.iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("missing queue row {id}"))
    };

    let unexplained_merge = row("placeholder-org/alpha#1");
    assert_eq!(unexplained_merge["kind"], "unexplained-write");
    assert_eq!(
        unexplained_merge["mandate"]["reason"],
        "unexplained write: machine-authored merge has no matching work order; merge gate fault: no verdict for merged head machine-unexplained-sha"
    );
    assert_eq!(
        unexplained_merge["mandate"]["scope_evidence"]["classification"],
        "unexplained"
    );
    for id in ["placeholder-org/alpha#2", "placeholder-org/alpha#3"] {
        assert_eq!(row(id)["kind"], "merge-gate-fault");
        assert!(
            row(id)["mandate"]["reason"]
                .as_str()
                .is_some_and(|reason| reason.starts_with("merge gate fault: no verdict"))
        );
    }

    let branch = row("placeholder-org/alpha@refs/heads/ostrom/unmatched");
    assert_eq!(branch["ref"], "@ostrom/unmatched");
    assert_eq!(branch["title"], "Pushed branch ostrom/unmatched");
    assert_eq!(branch["kind"], "unexplained-write");
    assert_eq!(
        branch["mandate"]["reason"],
        "unexplained write: pushed branch ostrom/unmatched has no matching work order"
    );
    assert_eq!(branch["opened"], "2026-08-01T00:00:00Z");
    assert_eq!(branch["age_days"], 0);
    assert_eq!(branch["aged_out"], false);
    assert_eq!(branch["needs_judgment"], false);
    assert_eq!(branch["blocked_by"], serde_json::json!([]));
    assert!(
        rows.iter().all(|row| {
            !matches!(
                row["id"].as_str(),
                Some("placeholder-org/alpha@refs/heads/ostrom/matched")
                    | Some("placeholder-org/alpha@refs/heads/ostrom/default")
                    | Some("placeholder-org/alpha@refs/heads/feature/outside")
            )
        }),
        "an excluded branch produced an alarm"
    );

    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(home.path().join("state.json")).expect("read sweep state"),
    )
    .expect("parse sweep state");
    let repo_state = &state["repos"]["placeholder-org/alpha"];
    assert_eq!(repo_state["merge_gate_fault_count"], 2);
    assert_eq!(repo_state["unexplained_write_count"], 2);
    assert_eq!(
        repo_state["unexplained_branch_writes"]["placeholder-org/alpha@refs/heads/ostrom/unmatched"]
            ["fingerprint"],
        "branch-v1|ostrom/unmatched|branch-sha"
    );
}

#[test]
fn degraded_branch_evidence_suppresses_reserved_namespace_alarms() {
    let home = tempdir().expect("temporary OSTROM_HOME");
    let mut repository = placeholder_repository();
    repository["branches"] = serde_json::json!([
        {"name": "ostrom/unmatched", "commit": {"sha": "branch-sha"}}
    ]);
    repository["branch_read_degraded"] = serde_json::json!(true);
    let fixture = write_placeholder_fixture(home.path(), repository);

    let output = run_placeholder_sweep(
        home.path(),
        &fixture,
        &["--started-at", "2026-08-01T00:00:00Z"],
    );
    assert!(output.status.success());
    let queue = fs::read_to_string(home.path().join("queue.jsonl")).expect("read queue");
    assert!(!queue.contains("@refs/heads/"));
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(home.path().join("state.json")).expect("read state"))
            .expect("parse state");
    assert_eq!(
        state["repos"]["placeholder-org/alpha"]["unexplained_branch_writes"],
        serde_json::json!({})
    );
}

#[test]
fn malformed_work_orders_warn_and_do_not_fail_the_sweep() {
    let home = tempdir().expect("temporary OSTROM_HOME");
    let fixture = write_placeholder_fixture(home.path(), placeholder_repository());
    fs::create_dir(home.path().join("work-orders")).expect("create work-order directory");
    fs::write(home.path().join("work-orders/malformed.json"), "not JSON")
        .expect("write malformed work order");

    let output = run_placeholder_sweep(
        home.path(),
        &fixture,
        &["--started-at", "2026-08-01T00:00:00Z"],
    );
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("ignoring malformed work order while classifying pushed branches")
    );
}

#[test]
fn fixture_refuses_a_full_second_branch_page() {
    let home = tempdir().expect("temporary OSTROM_HOME");
    let mut repository = placeholder_repository();
    repository["branches"] = serde_json::Value::Array(vec![
        serde_json::json!({"name": "feature/placeholder", "commit": {"sha": "placeholder-sha"}});
        200
    ]);
    let fixture = write_placeholder_fixture(home.path(), repository);

    let output = run_placeholder_sweep(
        home.path(),
        &fixture,
        &["--started-at", "2026-08-01T00:00:00Z"],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        "branch query for placeholder-org/alpha reached query_limit 200; refusing a truncated sweep"
    ));
    assert!(!home.path().join("queue.jsonl").exists());
}

#[cfg(unix)]
#[test]
fn github_worker_refuses_a_full_second_branch_page() {
    use std::{env, os::unix::fs::PermissionsExt};

    let home = tempdir().expect("temporary OSTROM_HOME");
    let bin = home.path().join("bin");
    fs::create_dir(&bin).expect("create fake binary directory");
    fs::write(home.path().join("mandates.yaml"), PLACEHOLDER_ROSTER)
        .expect("write placeholder roster");
    let fake_gh = bin.join("gh");
    fs::write(
        &fake_gh,
        r#"#!/usr/bin/env bash
set -eu
branch_page() {
  printf '['
  index=0
  while [ "$index" -lt 100 ]; do
    [ "$index" -eq 0 ] || printf ','
    printf '{"name":"feature/placeholder-%s","commit":{"sha":"placeholder-sha-%s"}}' "$index" "$index"
    index=$((index + 1))
  done
  printf ']\n'
}
case "$1 $2" in
  "auth status") exit 0 ;;
  "api -X")
    case "$*" in
      *"/issues?"*) printf 'HTTP/2 200 OK\r\netag: placeholder-etag\r\n\r\n[]' ;;
      *"/branches?"*) branch_page ;;
      *) exit 9 ;;
    esac
    ;;
  "api graphql")
    printf '%s\n' '{"data":{"repository":{"issues":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}'
    ;;
  "pr list") printf '%s\n' '[]' ;;
  "repo view") printf '%s\n' '{"defaultBranchRef":{"name":"main"}}' ;;
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
            "placeholder-org",
            "--mode",
            "full",
            "--started-at",
            "2026-08-01T00:00:00Z",
        ])
        .env("OSTROM_HOME", home.path())
        .env("PATH", path)
        .current_dir(home.path())
        .output()
        .expect("run GitHub worker");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        "branch query for placeholder-org/alpha reached query_limit 200; refusing a truncated sweep"
    ));
}

#[test]
fn realtime_clock_drives_sweep_and_cli_time_takes_precedence() {
    let home = tempdir().expect("temporary OSTROM_HOME");
    let mut repository = placeholder_repository();
    repository["branches"] = serde_json::json!([
        {"name": "ostrom/unmatched", "commit": {"sha": "branch-sha"}}
    ]);
    let fixture = write_placeholder_fixture(home.path(), repository);

    let before = DateTime::<Utc>::from(SystemTime::now());
    let realtime = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["sweep", "--fixture"])
        .arg(&fixture)
        .env("OSTROM_HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("run realtime sweep");
    let after = DateTime::<Utc>::from(SystemTime::now());
    assert!(realtime.status.success());
    let queue = fs::read_to_string(home.path().join("queue.jsonl")).expect("read realtime queue");
    let row: serde_json::Value =
        serde_json::from_str(queue.lines().next().expect("queue row")).expect("parse queue row");
    let opened = DateTime::parse_from_rfc3339(row["opened"].as_str().expect("opened timestamp"))
        .expect("parse opened timestamp")
        .with_timezone(&Utc);
    assert!(opened.timestamp() >= before.timestamp() && opened.timestamp() <= after.timestamp());

    let overridden = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "sweep",
            "--fixture",
            fixture.to_str().expect("fixture path is UTF-8"),
            "--started-at",
            "2026-08-03T04:05:06Z",
        ])
        .env("OSTROM_HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("run CLI-overridden sweep");
    assert!(overridden.status.success());
    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(home.path().join("state.json")).expect("read overridden state"),
    )
    .expect("parse overridden state");
    assert_eq!(state["last_full_reconciliation"], "2026-08-03T04:05:06Z");
}

#[test]
fn realtime_clock_drives_plan_and_cli_time_takes_precedence() {
    let home = tempdir().expect("temporary OSTROM_HOME");
    let fixture = write_placeholder_fixture(home.path(), placeholder_repository());
    let before = DateTime::<Utc>::from(SystemTime::now());
    let realtime = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["plan", "--fixture"])
        .arg(&fixture)
        .env("OSTROM_HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("run realtime plan");
    let after = DateTime::<Utc>::from(SystemTime::now());
    assert!(
        realtime.status.success(),
        "plan stderr: {}",
        String::from_utf8_lossy(&realtime.stderr)
    );
    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(home.path().join("state.json")).expect("read plan sweep state"),
    )
    .expect("parse plan sweep state");
    let reconciled = DateTime::parse_from_rfc3339(
        state["last_full_reconciliation"]
            .as_str()
            .expect("reconciliation timestamp"),
    )
    .expect("parse reconciliation timestamp")
    .with_timezone(&Utc);
    assert!(
        reconciled.timestamp() >= before.timestamp() && reconciled.timestamp() <= after.timestamp()
    );

    let overridden = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "plan",
            "--fixture",
            fixture.to_str().expect("fixture path is UTF-8"),
            "--started-at",
            "2026-08-05T06:07:08Z",
        ])
        .env("OSTROM_HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("run CLI-overridden plan");
    assert!(overridden.status.success());
}

#[test]
fn an_entirely_unmintable_roster_is_refused_without_overwriting() {
    let home = tempdir().expect("temporary OSTROM_HOME");
    fs::write(home.path().join("mandates.yaml"), PLACEHOLDER_ROSTER).expect("write roster");
    let queue_before = br##"{"id":"placeholder-org/alpha#7","repo":"placeholder-org/alpha","ref":"#7","title":"Placeholder retained decision","kind":"decision","mandate":{"reason":"placeholder"},"state":"deferred","opened":"2026-07-01T00:00:00Z","age_days":31,"aged_out":true,"needs_judgment":true,"blocked_by":[]}
"##;
    let state_before = br#"{"version":2,"sweep_mode":"full","repos":{"placeholder-org/alpha":{"cursor":"2026-07-01T00:00:00Z","records":{}}}}"#;
    fs::write(home.path().join("queue.jsonl"), queue_before).expect("write prior queue");
    fs::write(home.path().join("state.json"), state_before).expect("write prior state");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["sweep", "--started-at", "2026-08-01T00:00:00Z"])
        .env("OSTROM_HOME", home.path())
        .env("MANDATE_SECRETS_FILE", home.path().join("absent.yaml"))
        .current_dir(home.path())
        .output()
        .expect("run organization driver");
    assert!(
        !output.status.success(),
        "an entirely unmintable roster must refuse: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(home.path().join("queue.jsonl")).expect("read preserved queue"),
        queue_before
    );
    assert_eq!(
        fs::read(home.path().join("state.json")).expect("read preserved state"),
        state_before
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refusing to overwrite queue and state"));
    assert!(stderr.contains("acquisition succeeded for 0 of 1"));
    assert!(
        stderr.contains("for organization placeholder-org"),
        "stderr must name the organization: {stderr}"
    );
    assert!(
        stderr.contains("credentials unavailable"),
        "a missing secrets file is a named credential error, not a transport or scope one: {stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "a missing secrets file is a named error, not a panic: {stderr}"
    );
}
