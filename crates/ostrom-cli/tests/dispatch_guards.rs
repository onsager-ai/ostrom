#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tempfile::TempDir;

const ITEM_ID: &str = "example-org/example-repo#123";
const BRANCH: &str = "ostrom/123-placeholder";
const ORDER_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Fixture {
    root: TempDir,
    state: PathBuf,
    source: PathBuf,
    order: PathBuf,
    gh: PathBuf,
    systemd: PathBuf,
    codex: PathBuf,
    node: PathBuf,
    calls: PathBuf,
    now: u64,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("dispatch fixture");
        let state = root.path().join("ostrom");
        let source = root.path().join("source");
        let bin = root.path().join("bin");
        fs::create_dir_all(&state).expect("create state");
        fs::create_dir_all(&source).expect("create source");
        fs::create_dir_all(&bin).expect("create bin");
        git(&source, &["init", "-b", "main"]);
        git(
            &source,
            &["config", "user.email", "fixture@example.invalid"],
        );
        git(&source, &["config", "user.name", "Fixture"]);
        fs::write(source.join("README.md"), "placeholder\n").expect("write source fixture");
        git(&source, &["add", "README.md"]);
        git(&source, &["commit", "-m", "placeholder base"]);
        git(
            &source,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/example-org/example-repo.git",
            ],
        );
        git(&source, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        fs::write(
            state.join("mandates.yaml"),
            format!(
                "search_roots:\n  - {}\nprojects:\n  - repo: example-org/example-repo\n    delegated: []\n    excluded: []\n    reserved: []\n    default: excluded\n    paused: false\n    bounce: []\n",
                root.path().display()
            ),
        )
        .expect("write config");
        let order = root.path().join("order.json");
        fs::write(
            &order,
            format!(
                "{}\n",
                json!({
                    "schema_version": 1,
                    "item_id": ITEM_ID,
                    "repository": "example-org/example-repo",
                    "item_ref": "#123",
                    "branch_name": BRANCH,
                    "spec": "Implement the placeholder behavior.",
                    "acceptance_criteria": ["The placeholder behavior is observable."],
                    "constraints": ["Use placeholder data only."],
                    "order_id": ORDER_ID,
                    "created_at": "2026-08-01T00:00:00Z",
                    "cost_ceiling_usd": 20,
                    "token_ceiling": 500000
                })
            ),
        )
        .expect("write order");
        let gh = bin.join("gh-as");
        executable(
            &gh,
            r#"
while [ "$#" -gt 0 ] && [ "$1" != -- ]; do shift; done
[ "${1:-}" = -- ] || exit 98
shift
[ -z "${OSTROM_TEST_GH_LOG:-}" ] || printf '%s\n' "$*" >>"$OSTROM_TEST_GH_LOG"
if [ "$1 $2" = "gh api" ] && printf '%s' "$3" | grep -q '/branches?'; then
  page=${3##*page=}
  [ "${OSTROM_TEST_BRANCH_FAIL_PAGE:-}" != "$page" ] || exit 42
  if [ "${OSTROM_TEST_BRANCH_MALFORMED_PAGE:-}" = "$page" ]; then printf '%s\n' '{"unexpected":true}'; exit 0; fi
  if [ "$page" = 1 ]; then printf '%s\n' "${OSTROM_TEST_BRANCH_PAGE_1:-[]}"; else printf '%s\n' "${OSTROM_TEST_BRANCH_PAGE_2:-[]}"; fi
  exit 0
fi
if [ "$1 $2 $3" = "gh repo view" ]; then printf '%s\n' main; exit 0; fi
if [ "$1 $2" = "gh api" ] && printf '%s' "$3" | grep -q '/compare/'; then printf '%s\n' "${OSTROM_TEST_AHEAD:-0}"; exit 0; fi
if [ "$1 $2 $3" = "gh pr list" ]; then
  if printf '%s\n' "$*" | grep -q -- '--head'; then
    [ "${OSTROM_TEST_BRANCH_PR_FAIL:-0}" = 0 ] || exit 42
    printf '%s\n' "${OSTROM_TEST_BRANCH_PRS:-[]}"
  else
    printf '%s\n' "${OSTROM_TEST_OPEN_PRS:-[]}"
  fi
  exit 0
fi
if [ "$1 $2 $3" = "gh issue view" ]; then
  [ "${OSTROM_TEST_CLOSING_FAIL:-0}" = 0 ] || exit 42
  if [ -n "${OSTROM_TEST_CLOSING_REFS:-}" ]; then printf '%s\n' "$OSTROM_TEST_CLOSING_REFS"; else printf '%s\n' '{"closedByPullRequestsReferences":[]}'; fi
  exit 0
fi
if [ "$1 $2 $3" = "gh pr view" ]; then
  [ "${OSTROM_TEST_CLOSING_RESOLVE_FAIL:-0}" = 0 ] || exit 42
  if [ -n "${OSTROM_TEST_CLOSING_PR:-}" ]; then printf '%s\n' "$OSTROM_TEST_CLOSING_PR"; else printf '%s\n' '{"number":91,"state":"CLOSED","mergedAt":null,"url":"https://example.invalid/pull/91"}'; fi
  exit 0
fi
exit 97
"#,
        );
        let systemd = bin.join("systemd-run");
        executable(
            &systemd,
            "printf '%s\\n' called >>\"$OSTROM_TEST_SYSTEMD_CALLS\"",
        );
        let codex = bin.join("codex");
        executable(&codex, "[ \"${1:-}\" = --version ]");
        let node = bin.join("node");
        executable(&node, "exit 0");
        Self {
            root,
            state,
            source,
            order,
            gh,
            systemd,
            codex,
            node,
            calls: PathBuf::new(),
            now: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("current epoch")
                .as_secs(),
        }
        .with_calls()
    }

    fn with_calls(mut self) -> Self {
        self.calls = self.root.path().join("systemd.calls");
        self
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        let path = env::join_paths([
            self.node.parent().expect("bin parent").to_path_buf(),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ])
        .expect("fixture PATH");
        command
            .args(["dispatch", self.order.to_str().expect("UTF-8 order path")])
            .env_clear()
            .env("HOME", self.root.path())
            .env("PATH", path)
            .env("OSTROM_HOME", &self.state)
            .env("CLAUDE_CONFIG_DIR", &self.state)
            .env(
                "CLAUDE_PLUGIN_ROOT",
                workspace_root().join("plugins/ostrom"),
            )
            .env("MANDATE_IMPLEMENTER_SOURCE_REPO", &self.source)
            .env("MANDATE_GH_AS_BIN", &self.gh)
            .env("MANDATE_SYSTEMD_RUN_BIN", &self.systemd)
            .env("MANDATE_OSTROM_BIN", env!("CARGO_BIN_EXE_ostrom"))
            .env("CODEX_BIN", &self.codex)
            .env("OSTROM_TEST_SYSTEMD_CALLS", &self.calls);
        command
    }

    fn timestamp(&self) -> String {
        DateTime::<Utc>::from_timestamp(i64::try_from(self.now).expect("epoch fits i64"), 0)
            .expect("valid current epoch")
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    fn today(&self) -> String {
        self.timestamp()[..10].to_owned()
    }

    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(self.state.join("sprint.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("trace JSON"))
            .collect()
    }

    fn item_hash(&self) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
            .args(["work-order", "item-hash", ITEM_ID])
            .env_clear()
            .env("HOME", self.root.path())
            .output()
            .expect("calculate item hash");
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn lease(&self) -> PathBuf {
        self.state
            .join(format!("implementer-item-{}.lease", self.item_hash()))
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_owned()
}

fn executable(path: &Path, body: &str) {
    fs::write(path, format!("#!/usr/bin/env bash\nset -eu\n{body}\n")).expect("write stub");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
}

fn git(path: &Path, arguments: &[&str]) {
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(arguments)
            .status()
            .expect("run git")
            .success()
    );
}

fn default_page() -> String {
    json!([{"name":"main","commit":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}]).to_string()
}

fn matched_page() -> String {
    json!([
        {"name":"main","commit":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}},
        {"name":BRANCH,"commit":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}
    ])
    .to_string()
}

fn run(command: &mut Command) -> Output {
    command.output().expect("run dispatch")
}

fn assert_refused(output: &Output, code: i32, reason: &str) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(reason),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn source_roots_refuse_before_remote_reads_or_reservations() {
    for (config, reason) in [
        ("search_roots: []\n", "source-repository-roots-unconfigured"),
        (
            "search_roots:\n  - /placeholder/absent\n",
            "source-repository-not-found",
        ),
    ] {
        let fixture = Fixture::new();
        fs::write(fixture.state.join("mandates.yaml"), config).expect("replace root config");
        let gh_log = fixture.root.path().join("gh.calls");
        let output = run(fixture
            .command()
            .env_remove("MANDATE_IMPLEMENTER_SOURCE_REPO")
            .env("OSTROM_TEST_GH_LOG", &gh_log));
        assert_refused(&output, 3, reason);
        assert!(!gh_log.exists());
        assert!(!fixture.calls.exists());
        assert!(!fixture.lease().exists());
        let trace = fixture.trace();
        assert_eq!(trace.len(), 1);
        assert_eq!(trace[0]["kind"], "work-failed");
        assert_eq!(trace[0]["fact"]["reason"], reason);
        assert_eq!(trace[0]["fact"]["order_id"], ORDER_ID);
    }
}

#[test]
fn branch_listing_finds_exact_matches_across_pages_and_classifies_pr_state() {
    let page_one = matched_page();
    for (pulls, allowed) in [
        ("[]", false),
        (r#"[{"number":1,"state":"OPEN","mergedAt":null}]"#, false),
        (r#"[{"number":1,"state":"CLOSED","mergedAt":null}]"#, false),
        (
            r#"[{"number":1,"state":"MERGED","mergedAt":"2026-08-01T00:00:00Z"}]"#,
            true,
        ),
    ] {
        let fixture = Fixture::new();
        let output = run(fixture
            .command()
            .env("OSTROM_TEST_BRANCH_PAGE_1", &page_one)
            .env("OSTROM_TEST_BRANCH_PRS", pulls)
            .env("OSTROM_TEST_AHEAD", "4"));
        assert_eq!(output.status.success(), allowed, "{pulls}");
        if allowed {
            assert!(fixture.calls.exists());
            assert_eq!(fixture.trace()[0]["kind"], "work-dispatched");
        } else {
            assert_refused(&output, 3, "matched_key=branch_name");
            let row = &fixture.trace()[0];
            assert_eq!(row["fact"]["reason"], "branch-already-pushed");
            assert_eq!(row["fact"]["ahead_of_default"], 4);
            assert_eq!(row["fact"]["branch_listing"]["page_count"], 1);
        }
    }

    let fixture = Fixture::new();
    let full_page = (0..100)
        .map(|index| {
            json!({"name":format!("synthetic/{index}"),"commit":{"sha":"cccccccccccccccccccccccccccccccccccccccc"}})
        })
        .collect::<Vec<_>>();
    let second =
        json!([{"name":BRANCH,"commit":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}]);
    let output = run(fixture
        .command()
        .env(
            "OSTROM_TEST_BRANCH_PAGE_1",
            serde_json::to_string(&full_page).unwrap(),
        )
        .env("OSTROM_TEST_BRANCH_PAGE_2", second.to_string()));
    assert_refused(&output, 3, "matched_key=branch_name");
    assert_eq!(
        fixture.trace()[0]["fact"]["branch_listing"]["page_count"],
        2
    );
    assert_eq!(
        fixture.trace()[0]["fact"]["branch_listing"]["branch_count"],
        101
    );
}

#[test]
fn degraded_branch_evidence_fails_closed_with_a_named_trace() {
    for (variable, value, detail) in [
        ("OSTROM_TEST_BRANCH_FAIL_PAGE", "1", "page 1 failed"),
        (
            "OSTROM_TEST_BRANCH_MALFORMED_PAGE",
            "1",
            "returned JSON that is not a branch array",
        ),
    ] {
        let fixture = Fixture::new();
        let output = run(fixture.command().env(variable, value));
        assert_refused(&output, 1, detail);
        assert!(!fixture.calls.exists());
        let row = &fixture.trace()[0];
        assert_eq!(row["fact"]["reason"], "branch-listing-degraded");
        assert_eq!(row["fact"]["branch_listing"]["outcome"], "listing-degraded");
    }

    let fixture = Fixture::new();
    let output = run(fixture
        .command()
        .env("OSTROM_TEST_BRANCH_PAGE_1", matched_page())
        .env("OSTROM_TEST_BRANCH_PR_FAIL", "1"));
    assert_refused(&output, 1, "could not verify pull requests for branch");
    assert!(!fixture.calls.exists());

    let fixture = Fixture::new();
    let full_page = (0..100)
        .map(|index| {
            json!({"name":format!("synthetic/{index}"),"commit":{"sha":"cccccccccccccccccccccccccccccccccccccccc"}})
        })
        .collect::<Vec<_>>();
    let output = run(fixture
        .command()
        .env(
            "OSTROM_TEST_BRANCH_PAGE_1",
            serde_json::to_string(&full_page).unwrap(),
        )
        .env("OSTROM_TEST_BRANCH_FAIL_PAGE", "2"));
    assert_refused(&output, 1, "page 2 failed");
    assert!(!fixture.calls.exists());
    assert!(!fixture.lease().exists());
    let row = &fixture.trace()[0];
    assert_eq!(row["fact"]["reason"], "branch-listing-degraded");
    assert_eq!(row["fact"]["branch_listing"]["page_count"], 1);
    assert_eq!(row["fact"]["branch_listing"]["branch_count"], 100);
}

#[test]
fn a_numeric_branch_name_coincidence_is_not_identity_evidence() {
    let fixture = Fixture::new();
    let page = json!([
        {"name":"main","commit":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}},
        {"name":"chore/123-bump","commit":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}
    ]);
    let output = run(fixture
        .command()
        .env("OSTROM_TEST_BRANCH_PAGE_1", page.to_string()));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let row = &fixture.trace()[0];
    assert_eq!(
        row["fact"]["branch_listing"]["outcome"],
        "proven-exhaustive-no-match"
    );
    assert_eq!(row["fact"]["branch_listing"]["branch_count"], 2);
    assert!(row["fact"]["branch_listing"]["matched_branch"].is_null());
}

#[test]
fn closing_pull_requests_are_identity_keys_but_part_of_prose_is_not() {
    for state in ["OPEN", "MERGED"] {
        let fixture = Fixture::new();
        let url = "https://example.invalid/pull/91";
        let references = json!({"closedByPullRequestsReferences":[{"url":url}]}).to_string();
        let pull = json!({"number":91,"state":state,"mergedAt":null,"url":url}).to_string();
        let output = run(fixture
            .command()
            .env("OSTROM_TEST_BRANCH_PAGE_1", default_page())
            .env("OSTROM_TEST_CLOSING_REFS", references)
            .env("OSTROM_TEST_CLOSING_PR", pull));
        assert_refused(&output, 3, "matched_key=closing_pull_request");
        assert_eq!(
            fixture.trace()[0]["fact"]["matched_key"]["type"],
            "closing_pull_request"
        );
    }

    let fixture = Fixture::new();
    let output = run(
        fixture
            .command()
            .env("OSTROM_TEST_BRANCH_PAGE_1", default_page())
            .env(
                "OSTROM_TEST_OPEN_PRS",
                r#"[{"number":3,"title":"Partial implementation","body":"Part of #123 — one step","url":"https://example.invalid/pull/3"}]"#,
            ),
    );
    assert!(
        output.status.success(),
        "Part of prose must remain dispatchable: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn closing_pull_request_query_failures_fail_closed_before_reservation() {
    for (name, value, message) in [
        (
            "OSTROM_TEST_CLOSING_FAIL",
            "1",
            "could not verify closing pull requests",
        ),
        (
            "OSTROM_TEST_CLOSING_RESOLVE_FAIL",
            "1",
            "could not resolve closing pull request",
        ),
    ] {
        let fixture = Fixture::new();
        let references =
            json!({"closedByPullRequestsReferences":[{"url":"https://example.invalid/pull/91"}]});
        let output = run(fixture
            .command()
            .env("OSTROM_TEST_BRANCH_PAGE_1", default_page())
            .env("OSTROM_TEST_CLOSING_REFS", references.to_string())
            .env(name, value));
        assert_refused(&output, 1, message);
        assert!(!fixture.calls.exists());
        assert!(!fixture.lease().exists());
    }
}

#[test]
fn duplicate_and_concurrency_guards_release_the_new_item_lease() {
    let base_trace = |timestamp: &str, item: &str, order: &str| {
        format!(
            "{{\"ts\":{timestamp:?},\"kind\":\"work-dispatched\",\"fact\":{{\"item_id\":{item:?},\"order_id\":{order:?},\"unit_name\":\"ostrom-implementer-placeholder\",\"cost_ceiling_usd\":20,\"token_ceiling\":500000}},\"narration\":{{}}}}\n"
        )
    };
    for (items, extra, code, message) in [
        (
            vec![(ITEM_ID, "older-order")],
            None,
            3,
            "earlier work-dispatched row",
        ),
        (
            vec![
                ("example-org/other-repo#1", "other-one"),
                ("example-org/other-repo#2", "other-two"),
            ],
            None,
            3,
            "concurrency limit reached (2/2)",
        ),
        (
            vec![("example-org/example-repo#9", "same-repo")],
            Some(("MANDATE_MAX_IMPLEMENTERS", "6")),
            3,
            "per-repository concurrency limit reached",
        ),
    ] {
        let fixture = Fixture::new();
        let timestamp = fixture.timestamp();
        let trace = items
            .into_iter()
            .map(|(item, order)| base_trace(&timestamp, item, order))
            .collect::<String>();
        fs::write(fixture.state.join("sprint.jsonl"), trace).expect("write in-flight trace");
        let mut command = fixture.command();
        command.env("OSTROM_TEST_BRANCH_PAGE_1", default_page());
        if let Some((name, value)) = extra {
            command.env(name, value);
        }
        let output = run(&mut command);
        assert_refused(&output, code, message);
        assert!(!fixture.lease().exists());
        assert!(!fixture.calls.exists());
    }

    for invalid in ["0", "not-a-number"] {
        let fixture = Fixture::new();
        let output = run(fixture
            .command()
            .env("OSTROM_TEST_BRANCH_PAGE_1", default_page())
            .env("MANDATE_MAX_IMPLEMENTERS", invalid));
        assert_refused(
            &output,
            2,
            "MANDATE_MAX_IMPLEMENTERS must be a positive integer",
        );
        assert!(!fixture.lease().exists());
    }
}

#[test]
fn repository_capacity_reaps_only_stale_non_live_orders() {
    let stale_trace = |order: &str| {
        format!(
            "{{\"ts\":\"2026-07-31T00:00:00Z\",\"kind\":\"work-dispatched\",\"fact\":{{\"schema_version\":1,\"item_id\":\"example-org/example-repo#9\",\"order_id\":{order:?},\"unit_name\":\"ostrom-implementer-prior\",\"backend\":\"systemd\",\"cost_ceiling_usd\":1,\"token_ceiling\":1000}},\"narration\":{{}}}}\n"
        )
    };

    let missing = Fixture::new();
    fs::write(
        missing.state.join("sprint.jsonl"),
        stale_trace("placeholder-stale-order"),
    )
    .expect("write stale trace");
    let systemctl = missing.root.path().join("systemctl-stub");
    executable(
        &systemctl,
        "if [ \"${2:-}\" = show ]; then exit 4; fi\nexit 0",
    );
    let output = run(missing
        .command()
        .env("OSTROM_TEST_BRANCH_PAGE_1", default_page())
        .env("MANDATE_SYSTEMCTL_BIN", &systemctl)
        .env("MANDATE_IMPLEMENTER_STARTUP_GRACE_MILLISECONDS", "0"));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let trace = missing.trace();
    assert!(trace.iter().any(|row| {
        row["kind"] == "work-failed"
            && row["fact"]["order_id"] == "placeholder-stale-order"
            && row["fact"]["reason"] == "stale-order-reaped"
    }));
    assert!(trace.iter().any(|row| row["kind"] == "work-dispatched"));

    let live = Fixture::new();
    fs::write(
        live.state.join("sprint.jsonl"),
        stale_trace("placeholder-live-order"),
    )
    .expect("write live trace");
    let systemctl = live.root.path().join("systemctl-stub");
    executable(
        &systemctl,
        "printf '%s\\n' 'ActiveState=active' 'ExecMainCode=' 'ExecMainStatus=0'",
    );
    let output = run(live
        .command()
        .env("OSTROM_TEST_BRANCH_PAGE_1", default_page())
        .env("MANDATE_SYSTEMCTL_BIN", &systemctl));
    assert_refused(&output, 3, "per-repository concurrency limit reached");
    assert_eq!(live.trace().len(), 1);
    assert!(!live.calls.exists());
}

#[test]
fn a_live_item_lease_refuses_without_replacing_or_releasing_it() {
    let fixture = Fixture::new();
    let lease = fixture.lease();
    let contents = format!(
        "{{\"owner\":\"placeholder-holder\",\"started_at\":{},\"expires_at\":{}}}\n",
        fixture.now,
        fixture.now + 3_600,
    );
    fs::write(&lease, &contents).expect("write live item lease");
    let output = run(fixture
        .command()
        .env("OSTROM_TEST_BRANCH_PAGE_1", default_page()));
    assert_refused(&output, 3, "item already has a live implementer lease");
    assert!(!fixture.calls.exists());
    assert_eq!(fs::read_to_string(&lease).unwrap(), contents);
}

#[test]
fn repository_concurrency_overrides_and_other_repositories_leave_room() {
    for (item, roster_limit, env_limit) in [
        ("example-org/other-repo#1", None, None),
        ("example-org/example-repo#9", None, Some("2")),
        ("example-org/example-repo#9", Some(2), None),
    ] {
        let fixture = Fixture::new();
        fs::write(
            fixture.state.join("sprint.jsonl"),
            format!(
                "{{\"ts\":{:?},\"kind\":\"work-dispatched\",\"fact\":{{\"item_id\":{item:?},\"order_id\":\"prior\",\"unit_name\":\"ostrom-implementer-prior\",\"cost_ceiling_usd\":1,\"token_ceiling\":1000}},\"narration\":{{}}}}\n",
                fixture.timestamp(),
            ),
        )
        .expect("write concurrency trace");
        if let Some(limit) = roster_limit {
            fs::write(
                fixture.state.join("mandates.yaml"),
                format!(
                    "search_roots:\n  - {}\nprojects:\n  - repo: example-org/example-repo\n    max_implementers_per_repository: {limit}\n    delegated: []\n    excluded: []\n    reserved: []\n    default: excluded\n    paused: false\n    bounce: []\n",
                    fixture.root.path().display()
                ),
            )
            .expect("write roster override");
        }
        let mut command = fixture.command();
        command
            .env("OSTROM_TEST_BRANCH_PAGE_1", default_page())
            .env("MANDATE_MAX_IMPLEMENTERS", "6");
        if let Some(limit) = env_limit {
            command.env("MANDATE_MAX_IMPLEMENTERS_PER_REPOSITORY", limit);
        }
        let output = run(&mut command);
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(fixture.calls.exists());
        assert!(fixture.lease().exists());
    }
}

#[test]
fn projected_daily_spend_refuses_before_launch_and_releases_the_lease() {
    let fixture = Fixture::new();
    fs::write(
        fixture.state.join("sprint.jsonl"),
        format!(
            "{{\"ts\":\"{}T00:00:00Z\",\"kind\":\"pass-ended\",\"fact\":{{\"cost_usd\":31}},\"narration\":{{}}}}\n",
            fixture.today()
        ),
    )
    .expect("write spend trace");
    let output = run(fixture
        .command()
        .env("OSTROM_TEST_BRANCH_PAGE_1", default_page())
        .env("MANDATE_DAILY_CAP_USD", "50"));
    assert_refused(&output, 3, "daily spend cap would be exceeded");
    assert!(!fixture.calls.exists());
    assert!(!fixture.lease().exists());
}

#[test]
fn an_open_pr_reference_refuses_before_capacity_and_launch() {
    let fixture = Fixture::new();
    let output = run(
        fixture
            .command()
            .env("OSTROM_TEST_BRANCH_PAGE_1", default_page())
            .env(
                "OSTROM_TEST_OPEN_PRS",
                r#"[{"number":77,"title":"Placeholder","body":"Closes example-org/example-repo#123","url":"https://example.invalid/pull/77"}]"#,
            ),
    );
    assert_refused(&output, 3, "open pull request already references");
    assert!(!fixture.calls.exists());
    assert!(!fixture.lease().exists());
}

#[test]
fn dirty_and_ahead_mismatched_worktrees_are_preserved_before_remote_reads() {
    for ahead in [false, true] {
        let fixture = Fixture::new();
        let worktree = fixture
            .state
            .join("implementer-worktrees")
            .join(fixture.item_hash());
        fs::create_dir_all(worktree.parent().expect("worktree parent"))
            .expect("create worktree parent");
        git(
            &fixture.source,
            &[
                "worktree",
                "add",
                "-b",
                "old/placeholder-work",
                worktree.to_str().expect("UTF-8 worktree"),
                "refs/remotes/origin/main",
            ],
        );
        fs::write(worktree.join("preserved.txt"), "must survive\n").expect("write preserved work");
        if ahead {
            git(&worktree, &["add", "preserved.txt"]);
            git(&worktree, &["commit", "-m", "placeholder ahead work"]);
        }
        let gh_log = fixture.root.path().join("gh.calls");
        let output = run(fixture.command().env("OSTROM_TEST_GH_LOG", &gh_log));
        assert_refused(&output, 3, "worktree branch mismatch preserves work");
        assert!(!gh_log.exists());
        assert!(!fixture.calls.exists());
        assert!(!fixture.lease().exists());
        assert!(worktree.join("preserved.txt").exists());
        assert_eq!(
            Command::new("git")
                .args(["-C", worktree.to_str().unwrap(), "branch", "--show-current"])
                .output()
                .expect("read worktree branch")
                .stdout,
            b"old/placeholder-work\n"
        );
        let terminal = fixture.trace().pop().expect("mismatch trace");
        assert_eq!(terminal["fact"]["reason"], "worktree-branch-mismatch");
        assert_eq!(
            terminal["fact"]["worktree_path"],
            worktree.display().to_string()
        );
        if ahead {
            assert_eq!(terminal["fact"]["ahead_of_default"], 1);
        }
    }
}
