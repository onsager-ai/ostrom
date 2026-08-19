#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};

use serde_json::{Value, json};
use tempfile::tempdir;

fn git(cwd: &Path, arguments: &[&str]) -> Output {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(arguments)
        .output()
        .expect("run git fixture command");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn executable(path: &Path, source: &str) {
    fs::write(path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn repair_usage_and_repository_failures_match_the_shell_contract() {
    let fixture = tempdir().expect("temporary repair fixture");
    fs::write(
        fixture.path().join("mandates.yaml"),
        r#"provider: file
cadence_hours: 24
stuck_after_days: 7
search_roots: []
bounce_all: []
projects:
  - repo: placeholder-org/unreadable
    delegated: []
    excluded: []
    reserved: []
    default: delegated
    paused: false
    bounce: []
  - repo: placeholder-org/malformed
    delegated: []
    excluded: []
    reserved: []
    default: delegated
    paused: false
    bounce: []
  - repo: placeholder-org/truncated
    delegated: []
    excluded: []
    reserved: []
    default: delegated
    paused: false
    bounce: []
"#,
    )
    .unwrap();
    let truncated = fixture.path().join("truncated.json");
    fs::write(
        &truncated,
        serde_json::to_vec(&vec![json!({}); 1_000]).unwrap(),
    )
    .unwrap();
    let helper = fixture.path().join("credential-helper");
    executable(
        &helper,
        r#"#!/bin/sh
set -eu
repository="$2"
case "$repository" in
  placeholder-org/unreadable) exit 17 ;;
  placeholder-org/malformed) printf '%s\n' '{"unexpected":"listing shape"}' ;;
  placeholder-org/truncated) cat "$REPAIR_TRUNCATED" ;;
  *) exit 97 ;;
esac
"#,
    );

    let usage = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .arg("repair-prs")
        .env("OSTROM_HOME", fixture.path())
        .output()
        .unwrap();
    assert_eq!(usage.status.code(), Some(2));
    assert!(usage.stdout.is_empty());
    assert_eq!(
        usage.stderr,
        b"usage: repair-prs.sh <builder-lease-owner>\n"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["repair-prs", "builder-placeholder-wake1"])
        .env("OSTROM_HOME", fixture.path())
        .env("MANDATE_GH_AS_BIN", &helper)
        .env("REPAIR_TRUNCATED", &truncated)
        .env("MANDATE_TRACE_TIME", "2026-08-19T00:00:00Z")
        .current_dir(fixture.path())
        .output()
        .expect("run failed repair scan");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        output.stdout,
        b"{\"cap\":3,\"attempted\":0,\"repaired\":0,\"conflicted\":0,\"skipped\":0,\"failed\":0,\"repositories\":3,\"scanned_repositories\":0,\"repository_failures\":3}\n"
    );
    assert_eq!(
        output.stderr,
        concat!(
            "mandate repair: failed to enumerate open pull requests for placeholder-org/unreadable (rc=17)\n",
            "mandate repair: pull-request listing for placeholder-org/malformed was malformed\n",
            "mandate repair: pull-request listing for placeholder-org/truncated reached query limit 1000; refusing a truncated scan\n",
        )
        .as_bytes()
    );
    let traces = fs::read_to_string(fixture.path().join("sprint.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(traces.len(), 3);
    assert_eq!(
        traces
            .iter()
            .map(|row| row["fact"]["outcome"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![
            "enumeration-failed",
            "enumeration-malformed",
            "enumeration-truncated"
        ]
    );
}

#[test]
fn repair_only_pushes_the_builders_own_green_pr_with_an_ordinary_refspec() {
    let fixture = tempdir().expect("temporary published repair fixture");
    let home = fixture.path().join("home");
    let source = fixture.path().join("source");
    let remote = fixture.path().join("origin.git");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&source).unwrap();
    fs::write(
        home.join("mandates.yaml"),
        r#"provider: file
cadence_hours: 24
stuck_after_days: 7
search_roots: []
bounce_all: []
projects:
  - repo: placeholder-org/repair-repo
    delegated: []
    excluded: []
    reserved: []
    default: delegated
    paused: false
    bounce: []
"#,
    )
    .unwrap();
    git(&source, &["init", "-b", "main"]);
    git(&source, &["config", "user.name", "Placeholder Test"]);
    git(
        &source,
        &["config", "user.email", "placeholder@example.invalid"],
    );
    fs::write(source.join("initial.txt"), "initial\n").unwrap();
    git(&source, &["add", "initial.txt"]);
    git(&source, &["commit", "-m", "fixture initial"]);
    let initial = String::from_utf8(git(&source, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    let init_remote = Command::new("git")
        .args(["init", "--bare"])
        .arg(&remote)
        .output()
        .unwrap();
    assert!(init_remote.status.success());
    git(
        &source,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&source, &["push", "-u", "origin", "main"]);
    git(&source, &["switch", "-c", "builder-head", &initial]);
    fs::write(source.join("builder.txt"), "builder\n").unwrap();
    git(&source, &["add", "builder.txt"]);
    git(&source, &["commit", "-m", "fixture builder"]);
    let builder_head = String::from_utf8(git(&source, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    git(&source, &["push", "origin", "builder-head"]);
    git(&source, &["switch", "main"]);
    fs::write(source.join("base.txt"), "base\n").unwrap();
    git(&source, &["add", "base.txt"]);
    git(&source, &["commit", "-m", "fixture base"]);
    let base_head = String::from_utf8(git(&source, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    git(&source, &["push", "origin", "main"]);

    let listing = fixture.path().join("prs.json");
    fs::write(
        &listing,
        serde_json::to_vec(&json!([
            {
                "number": 1,
                "body": "Synthetic fixture.\n\nOstrom-Role: builder\n",
                "author": {"login": "ostrom-builder[bot]", "is_bot": true},
                "mergeable": "CONFLICTING",
                "headRefName": "builder-head",
                "baseRefName": "main",
                "headRefOid": builder_head,
                "isCrossRepository": false
            },
            {
                "number": 2,
                "body": "Synthetic fixture.\n\nOstrom-Role: builder\n",
                "author": {"login": "human", "is_bot": false},
                "mergeable": "CONFLICTING",
                "headRefName": "human-head",
                "baseRefName": "main",
                "headRefOid": "2222222222222222222222222222222222222222",
                "isCrossRepository": false
            },
            {
                "number": 3,
                "body": "Synthetic fixture.\n\nOstrom-Role: builder\n",
                "author": {"login": "ostrom-builder[bot]", "is_bot": true},
                "mergeable": "CONFLICTING",
                "headRefName": "failing-head",
                "baseRefName": "main",
                "headRefOid": "3333333333333333333333333333333333333333",
                "isCrossRepository": false
            }
        ]))
        .unwrap(),
    )
    .unwrap();
    let calls = fixture.path().join("calls");
    let helper = fixture.path().join("credential-helper");
    executable(
        &helper,
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >>"$REPAIR_CALLS"
shift 2
while [ "$1" != "--" ]; do shift; done
shift
if [ "$1 $2 $3" = "gh pr list" ]; then
  cat "$REPAIR_LISTING"
elif [ "$1 $2 $3" = "gh pr view" ]; then
  if [ "$4" = "1" ]; then
    printf '%s\n' '{"statusCheckRollup":[{"name":"test","conclusion":"SUCCESS","status":"COMPLETED"}]}'
  else
    printf '%s\n' '{"statusCheckRollup":[{"name":"test","conclusion":"FAILURE","status":"COMPLETED"}]}'
  fi
elif [ "$1 $2 $4" = "git -C fetch" ]; then
  exec git -C "$3" fetch --no-tags "$REPAIR_REMOTE" "$7" "$8"
elif [ "$1 $2 $4" = "git -C push" ]; then
  exec git -C "$3" push "$REPAIR_REMOTE" "$6"
else
  exit 96
fi
"#,
    );

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["repair-prs", "builder-placeholder-wake2"])
        .env("OSTROM_HOME", &home)
        .env("MANDATE_GH_AS_BIN", &helper)
        .env("REPAIR_LISTING", &listing)
        .env("REPAIR_CALLS", &calls)
        .env("REPAIR_REMOTE", &remote)
        .env("MANDATE_TRACE_TIME", "2026-08-19T00:01:00Z")
        .current_dir(fixture.path())
        .output()
        .expect("repair eligible published pull request");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["attempted"], 1);
    assert_eq!(summary["repaired"], 1);
    assert_eq!(summary["failed"], 0);
    let repaired_head = String::from_utf8(
        Command::new("git")
            .arg(format!("--git-dir={}", remote.display()))
            .args(["rev-parse", "builder-head"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_ne!(repaired_head.trim(), builder_head);
    let parents = String::from_utf8(
        Command::new("git")
            .arg(format!("--git-dir={}", remote.display()))
            .args(["rev-list", "--parents", "-n", "1", "builder-head"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let parents = parents.split_whitespace().collect::<Vec<_>>();
    assert_eq!(parents[1], builder_head);
    assert_eq!(parents[2], base_head);
    let calls = fs::read_to_string(calls).unwrap();
    assert!(calls.contains("gh pr view 1 "));
    assert!(calls.contains("gh pr view 3 "));
    assert!(!calls.contains("gh pr view 2 "));
    let push = calls
        .lines()
        .find(|line| line.contains(" git -C ") && line.contains(" push "))
        .unwrap();
    assert!(push.contains("HEAD:refs/heads/builder-head"));
    assert!(!push.contains("--force"));
    assert!(!push.contains(" -f "));
    let trace: Value = serde_json::from_str(
        fs::read_to_string(home.join("sprint.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(trace["fact"]["outcome"], "repaired");
    assert_eq!(trace["fact"]["owner"], "builder-placeholder-wake2");
}
