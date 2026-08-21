#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};

use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

const REPOSITORY: &str = "placeholder-org/alpha";
const NUMBER: &str = "7";
const CURRENT_HEAD: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const PREVIOUS_HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Fixture {
    root: TempDir,
    home: PathBuf,
    helper: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempdir().expect("temporary merge fixture");
        let home = root.path().join("home");
        fs::create_dir_all(&home).expect("create merge home");
        let helper = root.path().join("credential-helper");
        fs::write(
            &helper,
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$MERGE_TEST_LOG"
case "$*" in
  *" -- gh pr view "*)
    printf '{"headRefOid":"%s"}\n' "$MERGE_TEST_HEAD"
    ;;
  *" -- gh pr merge "*)
    printf 'merge child stdout\n'
    if [ "${MERGE_TEST_MERGE_EXIT:-0}" -ne 0 ]; then
      printf 'merge child stderr\n' >&2
      exit "$MERGE_TEST_MERGE_EXIT"
    fi
    ;;
  *) exit 97 ;;
esac
"#,
        )
        .expect("write credential helper");
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755))
            .expect("make credential helper executable");
        let log = root.path().join("calls.log");
        Self {
            root,
            home,
            helper,
            log,
        }
    }

    fn write_verdicts(&self, rows: &[Value]) {
        let text = rows
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(self.home.join("gate.jsonl"), format!("{text}\n"))
            .expect("write synthetic gate journal");
    }

    fn run(&self, merge_exit: Option<i32>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        command
            .args(["merge", REPOSITORY, NUMBER])
            .env_clear()
            .env("OSTROM_HOME", &self.home)
            .env("MANDATE_GH_AS_BIN", &self.helper)
            .env("MERGE_TEST_LOG", &self.log)
            .env("MERGE_TEST_HEAD", CURRENT_HEAD)
            .current_dir(self.root.path());
        if let Some(exit) = merge_exit {
            command.env("MERGE_TEST_MERGE_EXIT", exit.to_string());
        }
        command.output().expect("run merge wrapper")
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn verdict(head_sha: &str, verdict: &str) -> Value {
    json!({
        "pr": format!("{REPOSITORY}#{NUMBER}"),
        "head_sha": head_sha,
        "verdict": verdict,
        "already_judged": false,
    })
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("UTF-8 process output")
}

#[test]
fn pass_for_current_head_merges_through_exact_scopes_without_a_body() {
    let fixture = Fixture::new();
    fixture.write_verdicts(&[verdict(CURRENT_HEAD, "pass")]);

    let output = fixture.run(None);

    assert!(output.status.success());
    assert_eq!(output.stdout, b"merge child stdout\n");
    assert!(output.stderr.is_empty());
    assert_eq!(
        fixture.calls(),
        [
            format!(
                "gatekeeper {REPOSITORY} --repositories {REPOSITORY} --permissions metadata:read,pull_requests:read -- gh pr view {NUMBER} --repo {REPOSITORY} --json headRefOid"
            ),
            format!(
                "gatekeeper {REPOSITORY} --repositories {REPOSITORY} --permissions metadata:read,contents:write,pull_requests:write -- gh pr merge {NUMBER} --repo {REPOSITORY}"
            ),
        ]
    );
    assert!(!fixture.calls().iter().any(|call| call.contains("--body")));
}

#[test]
fn fail_inconclusive_and_no_verdict_refuse_without_attempting_merge() {
    for (recorded, expected) in [
        (Some("fail"), "verdict=fail"),
        (Some("inconclusive"), "verdict=inconclusive"),
        (None, "verdict=none (no verdict recorded)"),
    ] {
        let fixture = Fixture::new();
        if let Some(recorded) = recorded {
            fixture.write_verdicts(&[verdict(CURRENT_HEAD, recorded)]);
        }

        let output = fixture.run(None);

        assert_eq!(output.status.code(), Some(3), "{recorded:?}");
        assert!(output.stdout.is_empty());
        let stderr = text(&output.stderr);
        assert!(stderr.contains(CURRENT_HEAD), "{stderr}");
        assert!(stderr.contains(expected), "{stderr}");
        let calls = fixture.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].contains(" -- gh pr view "));
    }
}

#[test]
fn pass_for_previous_head_does_not_authorize_current_head() {
    let fixture = Fixture::new();
    fixture.write_verdicts(&[verdict(PREVIOUS_HEAD, "pass")]);

    let output = fixture.run(None);

    assert_eq!(output.status.code(), Some(3));
    let stderr = text(&output.stderr);
    assert!(stderr.contains(CURRENT_HEAD), "{stderr}");
    assert!(
        stderr.contains("verdict=none (no verdict recorded)"),
        "{stderr}"
    );
    assert_eq!(fixture.calls().len(), 1);
}

#[test]
fn attempted_github_merge_failure_has_its_own_exit_code() {
    let fixture = Fixture::new();
    fixture.write_verdicts(&[verdict(CURRENT_HEAD, "pass")]);

    let output = fixture.run(Some(19));

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(output.stdout, b"merge child stdout\n");
    let stderr = text(&output.stderr);
    assert!(stderr.contains("merge child stderr"), "{stderr}");
    assert!(stderr.contains(CURRENT_HEAD), "{stderr}");
    assert!(stderr.contains("merge attempt failed"), "{stderr}");
    assert_eq!(fixture.calls().len(), 2);
}

#[test]
fn malformed_arguments_print_usage_and_help_states_exit_codes() {
    for arguments in [
        vec!["merge"],
        vec!["merge", REPOSITORY],
        vec!["merge", "not-a-repository", NUMBER],
        vec!["merge", REPOSITORY, "07"],
        vec!["merge", REPOSITORY, NUMBER, "extra"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
            .args(arguments)
            .output()
            .expect("run invalid merge invocation");
        assert_eq!(output.status.code(), Some(64));
        let stderr = text(&output.stderr);
        assert!(
            stderr.contains("usage: ostrom merge <owner/repo> <pr-number>"),
            "{stderr}"
        );
        assert!(stderr.contains("3 = merge refused"), "{stderr}");
        assert!(stderr.contains("4 = merge attempted"), "{stderr}");
    }

    let help = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["merge", "--help"])
        .output()
        .expect("print merge help");
    assert!(help.status.success());
    let stdout = text(&help.stdout);
    assert!(
        stdout.contains("Usage: ostrom merge [ARGUMENTS]..."),
        "{stdout}"
    );
    assert!(stdout.contains("3  Merge refused"), "{stdout}");
    assert!(stdout.contains("4  Merge attempted"), "{stdout}");
}
