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

fn publish_fixture_branch(source: &Path, branch: &str, start: &str, file: &str) -> String {
    git(source, &["switch", "-c", branch, start]);
    fs::write(source.join(file), format!("{branch}\n")).unwrap();
    git(source, &["add", file]);
    git(source, &["commit", "-m", &format!("fixture {branch}")]);
    let head = String::from_utf8(git(source, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    git(source, &["push", "origin", branch]);
    head
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
fn repair_admits_red_heads_only_for_green_bases_and_preserves_scope_and_cap() {
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
    git(&source, &["switch", "main"]);
    fs::write(source.join("base.txt"), "base\n").unwrap();
    git(&source, &["add", "base.txt"]);
    git(&source, &["commit", "-m", "fixture base"]);
    let base_head = String::from_utf8(git(&source, &["rev-parse", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned();
    git(&source, &["push", "origin", "main"]);
    let red_base_head = publish_fixture_branch(&source, "red-base", &initial, "red-base.txt");
    let green_conflicting_head = publish_fixture_branch(
        &source,
        "green-conflicting-head",
        &initial,
        "green-conflicting.txt",
    );
    let red_mergeable_head =
        publish_fixture_branch(&source, "red-mergeable-head", &initial, "red-mergeable.txt");
    let green_cap_head =
        publish_fixture_branch(&source, "green-cap-head", &initial, "green-cap.txt");
    let skipped_cap_head =
        publish_fixture_branch(&source, "skipped-cap-head", &initial, "skipped-cap.txt");

    let listing = fixture.path().join("prs.json");
    fs::write(
        &listing,
        serde_json::to_vec(&json!([
            {
                "number": 1,
                "body": "Synthetic fixture.\n\nOstrom-Role: builder\n",
                "author": {"login": "ostrom-builder[bot]", "is_bot": true},
                "mergeable": "CONFLICTING",
                "headRefName": "green-conflicting-head",
                "baseRefName": "main",
                "headRefOid": green_conflicting_head,
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
                "headRefName": "red-red-head",
                "baseRefName": "red-base",
                "headRefOid": "3333333333333333333333333333333333333333",
                "isCrossRepository": false
            },
            {
                "number": 4,
                "body": "Synthetic fixture.\n\nOstrom-Role: builder\n",
                "author": {"login": "ostrom-builder[bot]", "is_bot": true},
                "mergeable": "MERGEABLE",
                "headRefName": "red-mergeable-head",
                "baseRefName": "main",
                "headRefOid": red_mergeable_head,
                "isCrossRepository": false
            },
            {
                "number": 5,
                "body": "Synthetic fixture.\n\nOstrom-Role: builder\n",
                "author": {"login": "ostrom-builder[bot]", "is_bot": true},
                "mergeable": "MERGEABLE",
                "headRefName": "green-current-head",
                "baseRefName": "main",
                "headRefOid": "5555555555555555555555555555555555555555",
                "isCrossRepository": false
            },
            {
                "number": 6,
                "body": "Synthetic fixture without a role marker.\n",
                "author": {"login": "ostrom-builder[bot]", "is_bot": true},
                "mergeable": "CONFLICTING",
                "headRefName": "missing-role-head",
                "baseRefName": "main",
                "headRefOid": "6666666666666666666666666666666666666666",
                "isCrossRepository": false
            },
            {
                "number": 7,
                "body": "Synthetic fixture.\n\nOstrom-Role: builder\n",
                "author": {"login": "ostrom-builder[bot]", "is_bot": true},
                "mergeable": "CONFLICTING",
                "headRefName": "fork-head",
                "baseRefName": "main",
                "headRefOid": "7777777777777777777777777777777777777777",
                "isCrossRepository": true
            },
            {
                "number": 8,
                "body": "Synthetic fixture.\n\nOstrom-Role: builder\n",
                "author": {"login": "ostrom-builder[bot]", "is_bot": true},
                "mergeable": "CONFLICTING",
                "headRefName": "green-cap-head",
                "baseRefName": "main",
                "headRefOid": green_cap_head,
                "isCrossRepository": false
            },
            {
                "number": 9,
                "body": "Synthetic fixture.\n\nOstrom-Role: builder\n",
                "author": {"login": "ostrom-builder[bot]", "is_bot": true},
                "mergeable": "CONFLICTING",
                "headRefName": "skipped-cap-head",
                "baseRefName": "main",
                "headRefOid": skipped_cap_head,
                "isCrossRepository": false
            },
            {
                "number": 10,
                "body": "Synthetic fixture.\n\nOstrom-Role: builder\n",
                "author": {"login": "ostrom-builder[bot]", "is_bot": true},
                "mergeable": "MERGEABLE",
                "headRefName": "red-current-head",
                "baseRefName": "main",
                "headRefOid": base_head,
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
  if [ "$4" = "3" ] || [ "$4" = "4" ] || [ "$4" = "10" ]; then
    printf '%s\n' '{"statusCheckRollup":[{"name":"test","conclusion":"FAILURE","status":"COMPLETED"}]}'
  else
    printf '%s\n' '{"statusCheckRollup":[{"name":"test","conclusion":"SUCCESS","status":"COMPLETED"}]}'
  fi
elif [ "$1 $2 $3" = "gh api graphql" ]; then
  qualified_name=
  for argument in "$@"; do
    case "$argument" in qualifiedName=*) qualified_name=${argument#qualifiedName=} ;; esac
  done
  if [ "$qualified_name" = "refs/heads/red-base" ]; then
    oid=$REPAIR_RED_BASE
    conclusion=FAILURE
  else
    oid=$REPAIR_GREEN_BASE
    conclusion=SUCCESS
  fi
  printf '%s\n' "{\"data\":{\"repository\":{\"ref\":{\"target\":{\"oid\":\"$oid\",\"statusCheckRollup\":{\"contexts\":{\"nodes\":[{\"conclusion\":\"$conclusion\",\"status\":\"COMPLETED\"}],\"pageInfo\":{\"hasNextPage\":false}}}}}}}}"
elif [ "$1 $2" = "gh api" ]; then
  case "$3" in
    *"$REPAIR_GREEN_BASE...$REPAIR_GREEN_BASE"*) status=identical ;;
    *) status=ahead ;;
  esac
  printf '%s\n' "{\"status\":\"$status\"}"
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
        .env("REPAIR_RED_BASE", &red_base_head)
        .env("REPAIR_GREEN_BASE", &base_head)
        .current_dir(fixture.path())
        .output()
        .expect("repair eligible published pull request");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["attempted"], 3);
    assert_eq!(summary["repaired"], 3);
    assert_eq!(summary["skipped"], 4);
    assert_eq!(summary["failed"], 0);
    let repaired_head = String::from_utf8(
        Command::new("git")
            .arg(format!("--git-dir={}", remote.display()))
            .args(["rev-parse", "green-conflicting-head"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert_ne!(repaired_head.trim(), green_conflicting_head);
    let parents = String::from_utf8(
        Command::new("git")
            .arg(format!("--git-dir={}", remote.display()))
            .args(["rev-list", "--parents", "-n", "1", "green-conflicting-head"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let parents = parents.split_whitespace().collect::<Vec<_>>();
    assert_eq!(parents[1], green_conflicting_head);
    assert_eq!(parents[2], base_head);
    let calls = fs::read_to_string(calls).unwrap();
    assert!(calls.contains("gh pr view 1 "));
    assert!(calls.contains("gh pr view 3 "));
    assert!(calls.contains("gh pr view 4 "));
    assert!(calls.contains("gh pr view 5 "));
    assert!(calls.contains("gh pr view 8 "));
    assert!(calls.contains("gh pr view 9 "));
    assert!(calls.contains("gh pr view 10 "));
    assert!(!calls.contains("gh pr view 2 "));
    assert!(!calls.contains("gh pr view 6 "));
    assert!(!calls.contains("gh pr view 7 "));
    assert_eq!(calls.matches("gh api graphql").count(), 3);
    assert_eq!(calls.matches(" gh api repos/").count(), 2);
    assert!(calls.contains(
        "--permissions metadata:read,pull_requests:read,checks:read,statuses:read -- gh api graphql"
    ));
    assert!(calls.contains(
        "--permissions metadata:read,contents:read -- gh api repos/placeholder-org/repair-repo/compare/"
    ));
    let pushes = calls
        .lines()
        .filter(|line| line.contains(" git -C ") && line.contains(" push "))
        .collect::<Vec<_>>();
    assert_eq!(pushes.len(), 3);
    assert!(
        pushes
            .iter()
            .any(|push| push.contains("HEAD:refs/heads/red-mergeable-head"))
    );
    assert!(pushes.iter().all(|push| !push.contains("--force")));
    assert!(pushes.iter().all(|push| !push.contains(" -f ")));

    let traces = fs::read_to_string(home.join("sprint.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let traces_for = |number: u64| {
        traces
            .iter()
            .filter(|trace| trace["fact"]["ref"] == format!("#{number}"))
            .collect::<Vec<_>>()
    };

    let green_conflicting = traces_for(1);
    assert_eq!(green_conflicting.len(), 1);
    assert_eq!(
        green_conflicting[0]["fact"],
        json!({
            "role": "builder",
            "owner": "builder-placeholder-wake2",
            "repo": "placeholder-org/repair-repo",
            "ref": "#1",
            "action": "merge-base-forward",
            "outcome": "repaired",
            "head_branch": "green-conflicting-head",
            "base_branch": "main",
            "head_sha": green_conflicting_head,
            "base_sha": base_head,
            "conflicted_paths": [],
            "cap": 3,
            "exit_code": 0
        })
    );
    assert_eq!(green_conflicting[0]["narration"], json!({}));

    let red_base = traces_for(3);
    assert_eq!(red_base.len(), 1);
    assert_eq!(red_base[0]["fact"]["outcome"], "red-head-red-base");
    assert_eq!(red_base[0]["fact"]["base_sha"], red_base_head);
    assert_eq!(
        red_base[0]["narration"]["reason"],
        "head and base branch checks are not green"
    );

    let red_mergeable = traces_for(4);
    assert_eq!(red_mergeable.len(), 1);
    assert_eq!(red_mergeable[0]["fact"]["outcome"], "repaired");
    assert_eq!(red_mergeable[0]["fact"]["head_sha"], red_mergeable_head);

    let not_behind = traces_for(5);
    assert_eq!(not_behind.len(), 1);
    assert_eq!(not_behind[0]["fact"]["outcome"], "green-head");

    assert!(traces_for(2).is_empty());
    assert!(traces_for(6).is_empty());
    assert!(traces_for(7).is_empty());

    let not_behind = traces_for(10);
    assert_eq!(not_behind.len(), 1);
    assert_eq!(not_behind[0]["fact"]["outcome"], "not-behind-base");
    assert_eq!(
        not_behind[0]["narration"]["reason"],
        "head already contains the checked base commit"
    );

    let skipped_cap = traces_for(9);
    assert_eq!(skipped_cap.len(), 1);
    assert_eq!(
        skipped_cap[0]["fact"],
        json!({
            "role": "builder",
            "owner": "builder-placeholder-wake2",
            "repo": "placeholder-org/repair-repo",
            "ref": "#9",
            "action": "merge-base-forward",
            "outcome": "skipped-cap",
            "head_branch": "skipped-cap-head",
            "base_branch": "main",
            "head_sha": skipped_cap_head,
            "base_sha": null,
            "conflicted_paths": [],
            "cap": 3
        })
    );
    assert_eq!(
        skipped_cap[0]["narration"]["reason"],
        "per-pass repair cap reached"
    );
}
