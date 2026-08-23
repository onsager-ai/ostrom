#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use chrono::{DateTime, Utc};
use ostrom_store::{AuditOptions, OstromPaths, audit, grant_excuse, list_excuses};
use serde_json::Value;
use tempfile::{TempDir, tempdir};

mod support;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/leaves");

// These expected bytes were captured by running the production scripts before
// their deletion. Keeping them as data makes parity durable after the shell is
// no longer available in the repository.
#[test]
fn audit_matches_recorded_shell_output_byte_for_byte() {
    let fixture = leaf_fixture();
    fs::copy(
        fixture_path("mandates.yaml"),
        fixture.home.path().join("mandates.yaml"),
    )
    .expect("copy mandates");
    fs::copy(
        fixture_path("gate.jsonl"),
        fixture.home.path().join("gate.jsonl"),
    )
    .expect("copy gate log");

    let output = run_injected_leaf(&fixture, "audit");
    assert_eq!(
        output,
        fs::read(fixture_path("audit.expected.txt")).unwrap()
    );
}

#[test]
fn excuse_grant_and_list_match_recorded_shell_output_byte_for_byte() {
    let fixture = leaf_fixture();
    let grant = run_injected_leaf(&fixture, "grant");
    let expected_grant = fs::read(fixture_path("excuse-grant.expected.jsonl")).unwrap();
    assert_eq!(grant, expected_grant);
    assert_eq!(
        fs::read(fixture.home.path().join("exceptions.jsonl")).unwrap(),
        expected_grant
    );

    let list = run_injected_leaf(&fixture, "list");
    assert_eq!(
        list,
        fs::read(fixture_path("excuse-list.expected.txt")).unwrap()
    );
}

fn run_injected_leaf(fixture: &LeafFixture, action: &str) -> Vec<u8> {
    let output_path = fixture.home.path().join(format!("{action}.output"));
    let output = Command::new(env::current_exe().expect("current integration test executable"))
        .args(["--exact", "injected_clock_leaf_child"])
        .env("OSTROM_TEST_LEAF_ACTION", action)
        .env("OSTROM_TEST_LEAF_HOME", fixture.home.path())
        .env("OSTROM_TEST_LEAF_CWD", &fixture.working_directory)
        .env("OSTROM_TEST_LEAF_OUTPUT", &output_path)
        .env("OSTROM_TEST_MERGED_PRS", fixture_path("merged-prs.json"))
        .env("PATH", fixture.path())
        .output()
        .expect("run injected-clock leaf child");
    assert_success(&output);
    fs::read(output_path).expect("read injected-clock leaf output")
}

#[test]
fn injected_clock_leaf_child() {
    let Ok(action) = env::var("OSTROM_TEST_LEAF_ACTION") else {
        return;
    };
    let home = PathBuf::from(env::var_os("OSTROM_TEST_LEAF_HOME").expect("leaf home"));
    let working_directory = PathBuf::from(env::var_os("OSTROM_TEST_LEAF_CWD").expect("leaf cwd"));
    let output_path = PathBuf::from(env::var_os("OSTROM_TEST_LEAF_OUTPUT").expect("leaf output"));
    let paths = OstromPaths {
        config: home.clone(),
        state: home,
    };
    let fixed = |value: &str| {
        DateTime::parse_from_rfc3339(value)
            .expect("fixed leaf timestamp")
            .with_timezone(&Utc)
    };
    let output = match action.as_str() {
        "audit" => audit(&AuditOptions {
            paths,
            working_directory,
            days: 30,
            audit_time: fixed("2026-08-01T00:00:00Z"),
        })
        .expect("run fixed-clock audit"),
        "grant" => grant_excuse(
            &paths,
            "placeholder-org/alpha#7",
            "review_threads",
            &["placeholder exception reason".to_owned()],
            Some(fixed("2026-08-01T12:00:00Z")),
        )
        .expect("run fixed-clock excuse grant"),
        "list" => {
            list_excuses(&paths, Some("placeholder-org/alpha#7")).expect("run excuse listing")
        }
        _ => panic!("unknown leaf child action {action}"),
    };
    fs::write(output_path, output).expect("write injected-clock leaf output");
}

#[test]
fn local_drift_matches_recorded_shell_output_byte_for_byte() {
    let fixture = leaf_fixture();
    fs::copy(
        fixture_path("local-drift-mandates.yaml"),
        fixture.home.path().join("mandates.yaml"),
    )
    .expect("copy local drift mandates");
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["local-drift", "--local-only"])
        .current_dir(&fixture.working_directory)
        .env("OSTROM_HOME", fixture.home.path())
        .env("PATH", path_with_system(fixture.path()))
        .output()
        .expect("run Rust local drift");
    assert_success(&output);
    assert_eq!(
        output.stdout,
        fs::read(fixture_path("local-drift.expected.txt")).unwrap()
    );
}

#[test]
fn malformed_leaf_input_has_the_shell_exit_status_and_never_panics() {
    let fixture = leaf_fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "excuse",
            "grant",
            "not-a-pr",
            "review_threads",
            "placeholder reason",
        ])
        .env("OSTROM_HOME", fixture.home.path())
        .env("PATH", fixture.path())
        .output()
        .expect("run malformed excuse grant");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("usage: ostrom excuse grant"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    assert!(!fixture.home.path().join("exceptions.jsonl").exists());
}

#[test]
fn excuse_grant_refuses_when_the_head_sha_is_unavailable() {
    let fixture = leaf_fixture();
    write_executable(
        &fixture.root.path().join("gh"),
        "#!/bin/sh\nif [ \"$1 $2\" = \"pr view\" ]; then printf '{\"headRefOid\":null}\\n'; exit 0; fi\nexit 1\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "excuse",
            "grant",
            "placeholder-org/alpha#7",
            "review_threads",
            "placeholder reason",
        ])
        .env("OSTROM_HOME", fixture.home.path())
        .env("PATH", fixture.path())
        .output()
        .expect("run excuse grant without a resolvable head");
    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("head SHA is unavailable"), "{stderr}");
    assert!(!fixture.home.path().join("exceptions.jsonl").exists());
}

#[test]
fn local_drift_invokes_only_read_operations_and_preserves_repository_state() {
    let fixture = tempdir().expect("temporary local drift fixture");
    let home = fixture.path().join("home");
    let repository = fixture.path().join("repository");
    let bin = fixture.path().join("bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin).unwrap();
    git(None, &["init", "-b", "main", repository.to_str().unwrap()]);
    git(
        Some(&repository),
        &["config", "user.name", "Placeholder User"],
    );
    git(
        Some(&repository),
        &["config", "user.email", "placeholder@example.invalid"],
    );
    fs::write(repository.join("tracked.txt"), "base\n").unwrap();
    git(Some(&repository), &["add", "tracked.txt"]);
    git(Some(&repository), &["commit", "-m", "placeholder base"]);
    git(
        Some(&repository),
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    git(Some(&repository), &["switch", "-c", "unpublished"]);
    fs::write(repository.join("leaf.txt"), "placeholder\n").unwrap();
    git(Some(&repository), &["add", "leaf.txt"]);
    git(
        Some(&repository),
        &["commit", "-m", "placeholder unpublished"],
    );

    fs::write(
        home.join("mandates.yaml"),
        format!(
            "provider: file\ncadence_hours: 24\nstuck_after_days: 7\nsearch_roots:\n  - {}\nbounce_all: []\nprojects: []\n",
            fixture.path().display()
        ),
    )
    .unwrap();
    let real_git = command_path("git");
    let git_log = fixture.path().join("git.log");
    write_executable(
        &bin.join("git"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >>\"$OSTROM_TEST_GIT_LOG\"\nexec \"$OSTROM_TEST_REAL_GIT\" \"$@\"\n",
    );
    let gh_log = fixture.path().join("gh.log");
    write_executable(
        &bin.join("gh"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >>\"$OSTROM_TEST_GH_LOG\"\nexit 97\n",
    );
    let before_refs = git_stdout(Some(&repository), &["show-ref"]);
    let before_status = git_stdout(
        Some(&repository),
        &["status", "--porcelain", "--untracked-files=normal"],
    );
    let before_tracked = fs::read(repository.join("tracked.txt")).unwrap();
    let before_leaf = fs::read(repository.join("leaf.txt")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["local-drift", "--local-only"])
        .current_dir(fixture.path())
        .env("OSTROM_HOME", &home)
        .env("PATH", &bin)
        .env("OSTROM_TEST_REAL_GIT", &real_git)
        .env("OSTROM_TEST_GIT_LOG", &git_log)
        .env("OSTROM_TEST_GH_LOG", &gh_log)
        .output()
        .expect("run read-only local drift");
    assert_success(&output);
    assert!(!gh_log.exists(), "--local-only must not invoke gh");
    let allowed = [
        "worktree",
        "status",
        "rev-parse",
        "for-each-ref",
        "rev-list",
        "cherry",
    ];
    for line in fs::read_to_string(&git_log).unwrap().lines() {
        let operation = line.split_whitespace().nth(2).expect("git -C operation");
        assert!(
            allowed.contains(&operation),
            "mutating or unknown git call: {line}"
        );
    }
    assert_eq!(git_stdout(Some(&repository), &["show-ref"]), before_refs);
    assert_eq!(
        git_stdout(
            Some(&repository),
            &["status", "--porcelain", "--untracked-files=normal"],
        ),
        before_status
    );
    assert_eq!(
        fs::read(repository.join("tracked.txt")).unwrap(),
        before_tracked
    );
    assert_eq!(fs::read(repository.join("leaf.txt")).unwrap(), before_leaf);
}

#[test]
fn granted_record_is_consumed_by_the_existing_sweep_join() {
    let fixture = leaf_fixture();
    fs::write(
        fixture.working_directory.join("ostrom.yaml"),
        "manifest_version: 1\n",
    )
    .expect("write repository policy");
    support::sign_manifest(&fixture.working_directory.join("ostrom.yaml"));
    fs::copy(
        fixture_path("mandates.yaml"),
        fixture.home.path().join("mandates.yaml"),
    )
    .expect("copy mandates");
    fs::write(
        fixture.home.path().join("gate.jsonl"),
        "{\"ts\":\"2026-07-01T00:00:00Z\",\"pr\":\"placeholder-org/alpha#1\",\"head_sha\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"evidence\":true,\"verdict\":\"pass\",\"conditions\":[]}\n",
    )
    .unwrap();
    let sweep_fixture = fixture.home.path().join("sweep.json");
    fs::write(
        &sweep_fixture,
        r#"{"repositories":[{"repo":"placeholder-org/alpha","issues":[],"open_prs":[],"merged_prs":[{"number":1,"title":"Placeholder floor","author":{"login":"placeholder-bot[bot]","isBot":true},"closingIssuesReferences":[],"createdAt":"2026-07-01T00:00:00Z","mergedAt":"2026-07-01T01:00:00Z","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"MERGED"},{"number":7,"title":"Placeholder exception join","author":{"login":"placeholder-bot[bot]","isBot":true},"closingIssuesReferences":[],"createdAt":"2026-07-02T00:00:00Z","mergedAt":"2026-07-03T00:00:00Z","headRefOid":"1111111111111111111111111111111111111111","state":"MERGED"}],"default_branch":null,"ci_runs":[]}]}"#,
    )
    .unwrap();

    let baseline = run_fixture_sweep(
        fixture.home.path(),
        &fixture.working_directory,
        &sweep_fixture,
    );
    assert_success(&baseline);
    let baseline_queue = fs::read_to_string(fixture.home.path().join("queue.jsonl")).unwrap();
    assert!(
        queue_has_merge_fault(&baseline_queue),
        "the fixture must produce a merge fault without an exception: {baseline_queue}"
    );
    fs::remove_file(fixture.home.path().join("queue.jsonl")).unwrap();
    fs::remove_file(fixture.home.path().join("state.json")).unwrap();

    let grant = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "excuse",
            "grant",
            "placeholder-org/alpha#7",
            "merge_protocol",
            "placeholder merge explanation",
        ])
        .env("OSTROM_HOME", fixture.home.path())
        .env("PATH", fixture.path())
        .output()
        .expect("grant merge protocol exception");
    assert_success(&grant);
    let sweep = run_fixture_sweep(
        fixture.home.path(),
        &fixture.working_directory,
        &sweep_fixture,
    );
    assert_success(&sweep);
    let queue = fs::read_to_string(fixture.home.path().join("queue.jsonl")).unwrap();
    assert!(
        !queue_has_merge_fault(&queue),
        "the exact exception record must suppress the matching merge-protocol fault: {queue}"
    );
}

fn run_fixture_sweep(home: &Path, working_directory: &Path, fixture: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "sweep",
            "--mode",
            "full",
            "--fixture",
            fixture.to_str().unwrap(),
            "--started-at",
            "2026-07-04T00:00:00Z",
        ])
        .current_dir(working_directory)
        .env("OSTROM_HOME", home)
        .env(
            "OSTROM_POLICY_TRUSTED_KEYS",
            working_directory.join("trusted-policy-keys"),
        )
        .output()
        .expect("run fixture sweep")
}

fn queue_has_merge_fault(queue: &str) -> bool {
    queue
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|row| {
            row.get("id").and_then(Value::as_str) == Some("placeholder-org/alpha#7")
                && matches!(
                    row.get("kind").and_then(Value::as_str),
                    Some("merge-gate-fault" | "unexplained-write")
                )
        })
}

struct LeafFixture {
    root: TempDir,
    home: TempDir,
    working_directory: PathBuf,
}

impl LeafFixture {
    fn path(&self) -> &Path {
        self.root.path()
    }
}

fn leaf_fixture() -> LeafFixture {
    let root = tempdir().expect("temporary command directory");
    let home = tempdir().expect("temporary OSTROM_HOME");
    let working_directory = root.path().join("working");
    fs::create_dir_all(&working_directory).unwrap();
    write_executable(
        &root.path().join("gh"),
        "#!/bin/sh\nif [ \"$1 $2\" = \"auth status\" ]; then exit 0; fi\nif [ \"$1 $2\" = \"pr list\" ]; then exec /bin/cat \"$OSTROM_TEST_MERGED_PRS\"; fi\nif [ \"$1 $2\" = \"pr view\" ]; then printf '{\"headRefOid\":\"1111111111111111111111111111111111111111\"}\\n'; exit 0; fi\nexit 1\n",
    );
    LeafFixture {
        root,
        home,
        working_directory,
    }
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(FIXTURES).join(name)
}

fn path_with_system(first: &Path) -> String {
    format!(
        "{}:{}",
        first.display(),
        std::env::var("PATH").expect("test PATH")
    )
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn command_path(name: &str) -> PathBuf {
    let output = Command::new("sh")
        .args(["-c", &format!("command -v {name}")])
        .output()
        .unwrap();
    assert_success(&output);
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

fn git(directory: Option<&Path>, args: &[&str]) {
    let output = git_command(directory, args).output().unwrap();
    assert_success(&output);
}

fn git_stdout(directory: Option<&Path>, args: &[&str]) -> Vec<u8> {
    let output = git_command(directory, args).output().unwrap();
    assert_success(&output);
    output.stdout
}

fn git_command(directory: Option<&Path>, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    if let Some(directory) = directory {
        command.arg("-C").arg(directory);
    }
    command.args(args);
    command
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
