#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use ostrom_core::WorkOrder;
use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    state: PathBuf,
    source: PathBuf,
    worktree: PathBuf,
    branch: String,
    gh: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("reaper fixture");
        let state = root.path().join("ostrom");
        let source = root.path().join("source");
        fs::create_dir_all(state.join("work-orders")).expect("create state");
        fs::create_dir_all(&source).expect("create source");
        git(&source, &["init", "-b", "main"]);
        git(
            &source,
            &["config", "user.email", "fixture@example.invalid"],
        );
        git(&source, &["config", "user.name", "Fixture"]);
        fs::write(source.join("README.md"), "placeholder\n").expect("write source");
        git(&source, &["add", "README.md"]);
        git(&source, &["commit", "-m", "placeholder base"]);

        let branch = "ostrom/7-placeholder".to_owned();
        let order_value = json!({
            "schema_version": 1,
            "item_id": "placeholder-org/alpha#7",
            "repository": "placeholder-org/alpha",
            "item_ref": "#7",
            "branch_name": branch,
            "spec": "Change the placeholder fixture.",
            "acceptance_criteria": ["The placeholder changes."],
            "constraints": ["Remain inside the fixture."],
            "order_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "created_at": "2026-08-01T00:00:00Z",
            "cost_ceiling_usd": 10,
            "token_ceiling": 100
        });
        let order = WorkOrder::from_json(order_value.to_string().as_bytes()).expect("valid order");
        fs::write(
            state
                .join("work-orders")
                .join(format!("{}.json", order.item_hash())),
            format!("{order_value}\n"),
        )
        .expect("write order");
        let worktree = state.join("implementer-worktrees").join(order.item_hash());
        fs::create_dir_all(worktree.parent().expect("worktree parent"))
            .expect("create worktree parent");
        git(
            &source,
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                worktree.to_str().expect("UTF-8 worktree"),
                "main",
            ],
        );
        let gh = root.path().join("gh-as");
        executable(
            &gh,
            r#"
while [ "$#" -gt 0 ] && [ "$1" != -- ]; do shift; done
shift
[ -z "${OSTROM_TEST_GH_LOG:-}" ] || printf '%s\n' "$*" >>"$OSTROM_TEST_GH_LOG"
if [ "$1 $2" = "gh api" ]; then printf '%s\n' "${OSTROM_TEST_REFS:-[]}"; exit 0; fi
if [ "$1 $2 $3" = "gh pr list" ]; then
  if printf '%s\n' "$*" | grep -q -- '--head'; then printf '%s\n' "${OSTROM_TEST_PRS:-[]}"; else printf '%s\n' "${OSTROM_TEST_OPEN_PRS:-[]}"; fi
  exit 0
fi
if [ "$1 $2 $3" = "gh issue view" ]; then
  if [ -n "${OSTROM_TEST_ISSUE:-}" ]; then printf '%s\n' "$OSTROM_TEST_ISSUE"; else printf '%s\n' '{"state":"OPEN","closedByPullRequestsReferences":[]}'; fi
  exit 0
fi
if [ "$1 $2 $3" = "gh pr view" ]; then printf '%s\n' "$OSTROM_TEST_PR_VIEW"; exit 0; fi
exit 97
"#,
        );
        Self {
            root,
            state,
            source,
            worktree,
            branch,
            gh,
        }
    }

    fn command(&self, apply: bool) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        command.arg("reap-worktrees");
        if apply {
            command.arg("--apply");
        }
        command
            .env_clear()
            .env("HOME", self.root.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("OSTROM_HOME", &self.state)
            .env("MANDATE_GH_AS_BIN", &self.gh)
            .env("MANDATE_LEASE_NOW_EPOCH", "100")
            .env("MANDATE_TRACE_TIME", "2026-08-01T00:00:00Z");
        command
    }

    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(self.state.join("sprint.jsonl"))
            .expect("read trace")
            .lines()
            .map(|line| serde_json::from_str(line).expect("trace JSON"))
            .collect()
    }
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
            .success(),
        "git {arguments:?}"
    );
}

fn run(command: &mut Command) -> Output {
    command.output().expect("run reaper")
}

fn output_rows(output: &Output) -> Vec<Value> {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("output JSON"))
        .collect()
}

#[test]
fn merged_remote_absent_worktree_is_dry_run_then_reaped_with_byte_totals() {
    let fixture = Fixture::new();
    let dry = run(fixture.command(false).env(
        "OSTROM_TEST_PRS",
        r#"[{"number":7,"state":"MERGED","url":"https://example.invalid/pull/7"}]"#,
    ));
    let rows = output_rows(&dry);
    assert_eq!(rows[0]["outcome"], "would-reap");
    assert_eq!(
        rows[0]["reason"],
        "pull-request-resolved-remote-branch-absent"
    );
    assert_eq!(rows[0]["reclaimed_bytes"], 0);
    assert!(rows[0]["bytes"].as_u64().unwrap() > 0);
    assert!(fixture.worktree.exists());

    let applied = run(fixture.command(true).env(
        "OSTROM_TEST_PRS",
        r#"[{"number":7,"state":"MERGED","url":"https://example.invalid/pull/7"}]"#,
    ));
    let rows = output_rows(&applied);
    assert_eq!(rows[0]["outcome"], "reaped");
    assert_eq!(rows[0]["reclaimed_bytes"], rows[0]["bytes"]);
    assert!(rows[1]["reclaimed_bytes"].as_u64().unwrap() > 0);
    assert_eq!(rows[1]["reaped_count"], 1);
    assert!(!fixture.worktree.exists());
    assert!(
        !Command::new("git")
            .arg("-C")
            .arg(&fixture.source)
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", fixture.branch),
            ])
            .status()
            .expect("inspect branch")
            .success()
    );
    let trace = fixture.trace();
    assert_eq!(trace[0]["kind"], "worktree-reaped");
    assert_eq!(trace[0]["fact"]["outcome"], "would-reap");
    assert_eq!(trace[2]["kind"], "worktree-reaped");
    assert_eq!(trace[2]["fact"]["outcome"], "reaped");
    assert!(trace[3]["fact"]["reclaimed_bytes"].as_u64().unwrap() > 0);
}

#[test]
fn dirty_closed_item_is_retained_without_consulting_github() {
    let fixture = Fixture::new();
    fs::write(fixture.worktree.join("preserved.txt"), "expensive result\n")
        .expect("write dirty work");
    let gh_log = fixture.root.path().join("gh.calls");
    let output = run(fixture
        .command(true)
        .env("OSTROM_TEST_GH_LOG", &gh_log)
        .env(
            "OSTROM_TEST_ISSUE",
            r#"{"state":"CLOSED","closedByPullRequestsReferences":[]}"#,
        ));
    let rows = output_rows(&output);
    assert_eq!(rows[0]["outcome"], "retained");
    assert_eq!(rows[0]["reason"], "dirty-worktree");
    assert_eq!(rows[0]["reclaimed_bytes"], 0);
    assert!(fixture.worktree.join("preserved.txt").exists());
    assert!(!gh_log.exists());
    assert_eq!(fixture.trace()[0]["kind"], "worktree-retained");
}

#[test]
fn remote_branch_is_retained_even_when_the_local_worktree_is_clean() {
    let fixture = Fixture::new();
    let output = run(fixture.command(true).env(
        "OSTROM_TEST_REFS",
        json!([{"ref":format!("refs/heads/{}", fixture.branch)}]).to_string(),
    ));
    let rows = output_rows(&output);
    assert_eq!(rows[0]["outcome"], "retained");
    assert_eq!(rows[0]["reason"], "remote-branch-present");
    assert!(fixture.worktree.exists());
    assert_eq!(rows[1]["retained_count"], 1);
    assert_eq!(rows[1]["reclaimed_bytes"], 0);
}

#[test]
fn closed_item_without_an_open_pull_request_is_reaped() {
    let fixture = Fixture::new();
    let output = run(fixture.command(true).env(
        "OSTROM_TEST_ISSUE",
        r#"{"state":"CLOSED","closedByPullRequestsReferences":[]}"#,
    ));
    let rows = output_rows(&output);
    assert_eq!(rows[0]["outcome"], "reaped");
    assert_eq!(rows[0]["reason"], "item-closed-no-open-pull-request");
    assert!(!fixture.worktree.exists());
}

#[test]
fn open_pull_request_for_a_closed_item_retains_the_worktree() {
    let fixture = Fixture::new();
    let output = run(fixture
        .command(true)
        .env(
            "OSTROM_TEST_ISSUE",
            r#"{"state":"CLOSED","closedByPullRequestsReferences":[]}"#,
        )
        .env(
            "OSTROM_TEST_OPEN_PRS",
            r#"[{"number":9,"title":"Continue placeholder","body":"Part of placeholder-org/alpha#7","url":"https://example.invalid/pull/9"}]"#,
        ));
    let rows = output_rows(&output);
    assert_eq!(rows[0]["outcome"], "retained");
    assert_eq!(rows[0]["reason"], "pull-request-open");
    assert!(fixture.worktree.exists());
}

#[test]
fn unreadable_git_status_is_treated_as_dirty_and_retained() {
    let fixture = Fixture::new();
    fs::write(fixture.worktree.join(".git"), "gitdir: /does/not/exist\n")
        .expect("break worktree git link");
    let gh_log = fixture.root.path().join("gh.calls");
    let output = run(fixture.command(true).env("OSTROM_TEST_GH_LOG", &gh_log));
    let rows = output_rows(&output);
    assert_eq!(rows[0]["outcome"], "retained");
    assert_eq!(rows[0]["reason"], "git-status-unreadable");
    assert!(fixture.worktree.exists());
    assert!(!gh_log.exists());
}

#[test]
fn a_live_implementer_lease_retains_the_worktree() {
    let fixture = Fixture::new();
    let item_hash = fixture
        .worktree
        .file_name()
        .expect("item hash")
        .to_string_lossy();
    fs::write(
        fixture
            .state
            .join(format!("implementer-item-{item_hash}.lease")),
        r#"{"owner":"ostrom-implementer-placeholder","started_at":50,"expires_at":200}
"#,
    )
    .expect("write live lease");
    let gh_log = fixture.root.path().join("gh.calls");
    let output = run(fixture.command(true).env("OSTROM_TEST_GH_LOG", &gh_log));
    let rows = output_rows(&output);
    assert_eq!(rows[0]["outcome"], "retained");
    assert_eq!(rows[0]["reason"], "live-implementer-lease");
    assert!(fixture.worktree.exists());
    assert!(!gh_log.exists());
}
