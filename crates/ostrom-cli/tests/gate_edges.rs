#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use ostrom_core::PolicyManifest;
use ostrom_store::policy_manifest_digest;
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    home: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("gate edge fixture");
        let home = root.path().join("ostrom");
        let bin = root.path().join("bin");
        fs::create_dir_all(&home).expect("create gate home");
        fs::create_dir_all(&bin).expect("create gate bin");
        fs::write(
            home.join("gate.yaml"),
            r#"provider: file
bounce_all: []
projects:
  - repo: example-org/example-repo
    required_checks: [verify-*]
    bounce: [label:risk:protected-surface, substance:fly-spend]
    reserved: []
"#,
        )
        .expect("write gate config");
        executable(
            &bin.join("gh"),
            r#"
mode=${OSTROM_TEST_GATE_MODE:-pass}
check=success
case "$mode" in
  check-failure) check=failure ;;
  check-running) check='' ;;
  unknown-check) check=unrecognized ;;
esac
if [ "$1 $2" = "pr view" ]; then
  mergeable=MERGEABLE
  draft=false
  title='fix(core): safe placeholder change'
  case "$mode" in
    unknown-mergeable) mergeable=UNKNOWN ;;
    draft) draft=true ;;
    bounce) title='feat(core): protected placeholder change' ;;
  esac
  if [ "$mode" = "metadata-403" ]; then
    printf '%s\n' 'GraphQL: Resource not accessible by integration (repository.pullRequest)' >&2
    exit 1
  fi
  jq -cn --argjson number "$3" --arg head "${OSTROM_TEST_GATE_HEAD:-aaaaaaaaaaaaaaaa}" \
    --arg mergeable "$mergeable" --argjson draft "$draft" --arg title "$title" --arg check "$check" '
    {number:$number,title:$title,author:{login:"builder-login"},headRefOid:$head,
     labels:(if env.OSTROM_TEST_GATE_MODE == "bounce" then [{name:"risk:protected-surface"}] else [] end),
     closingIssuesReferences:[],mergeable:$mergeable,isDraft:$draft}
    | if env.OSTROM_TEST_GATE_MODE == "missing-mergeable" then del(.mergeable)
      elif env.OSTROM_TEST_GATE_MODE == "malformed-mergeable" then .mergeable=7 else . end'
  exit 0
fi
if [ "$1" = "api" ] && [[ "$2" == repos/example-org/example-repo/commits/*/check-runs\?* ]]; then
  if [ "$mode" = "check-runs-403" ]; then
    printf '%s\n' 'HTTP 403: Resource not accessible by integration' >&2
    exit 1
  fi
  if [ "$mode" = "no-check-runs" ] || [ "$mode" = "status-check" ]; then
    printf '%s\n' '{"total_count":0,"check_runs":[]}'
    exit 0
  fi
  if [ "$mode" = "check-running" ]; then
    printf '%s\n' '{"total_count":1,"check_runs":[{"name":"verify-linux","status":"in_progress","conclusion":null,"app":{"slug":"github-actions"}}]}'
    exit 0
  fi
  jq -cn --arg check "$check" \
    '{total_count:1,check_runs:[{name:"verify-linux",status:"completed",conclusion:$check,app:{slug:"github-actions"}}]}'
  exit 0
fi
if [ "$1" = "api" ] && [[ "$2" == repos/example-org/example-repo/commits/*/status\?* ]]; then
  if [ "$mode" = "status-403" ]; then
    printf '%s\n' 'HTTP 403: Resource not accessible by integration' >&2
    exit 1
  fi
  if [ "$mode" = "status-check" ]; then
    printf '%s\n' '{"total_count":1,"statuses":[{"context":"verify-linux","state":"success"}]}'
    exit 0
  fi
  printf '%s\n' '{"total_count":0,"statuses":[]}'
  exit 0
fi
if [ "$1 $2" = "pr diff" ]; then
  if printf '%s\n' "$*" | grep -q -- '--name-only'; then
    case "$mode" in fly-*|diff-error) printf '%s\n' deploy/fly.toml ;; policy-path) printf '%s\n' ostrom.yaml ;; *) printf '%s\n' src/placeholder.rs ;; esac
    exit 0
  fi
  case "$mode" in
    diff-error) printf '%s\n' 'placeholder diff unavailable' >&2; exit 1 ;;
    fly-env) body=' [env]\n-region = "a"\n+region = "b"' ;;
    fly-machine) body=' [[vm]]\n-size = "shared"\n+size = "performance"' ;;
    fly-count) body='-count = 1\n+count = 2' ;;
    fly-region) body='-region = "a"\n+region = "b"' ;;
    fly-scaling) body='+[scaling]\n+count = 2' ;;
    *) body='-placeholder\n+safe' ;;
  esac
  printf 'diff --git a/deploy/fly.toml b/deploy/fly.toml\n--- a/deploy/fly.toml\n+++ b/deploy/fly.toml\n@@ -1 +1 @@\n%b\n' "$body"
  exit 0
fi
if [ "$1 $2" = "api graphql" ]; then
  case "$mode" in
    thread-author) thread='{"id":"THREAD_one","isResolved":true,"resolvedBy":{"login":"builder-login"},"comments":{"nodes":[{"author":{"login":"reviewer"}}]}}' ;;
    thread-unanswered) thread='{"id":"THREAD_one","isResolved":false,"resolvedBy":null,"comments":{"nodes":[{"author":{"login":"reviewer"}}]}}' ;;
    thread-answered) thread='{"id":"THREAD_one","isResolved":false,"resolvedBy":null,"comments":{"nodes":[{"author":{"login":"builder-login"}}]}}' ;;
    *) thread='' ;;
  esac
  if [ -n "$thread" ]; then nodes="[$thread]"; else nodes='[]'; fi
  printf '{"data":{"repository":{"pullRequest":{"author":{"login":"builder-login"},"reviewThreads":{"nodes":%s,"pageInfo":{"hasNextPage":false,"endCursor":"cursor"}}}}}}\n' "$nodes"
  exit 0
fi
exit 97
"#,
        );
        Self { root, home, bin }
    }

    fn run(&self, mode: &str, number: u64, head: &str) -> Output {
        let path = env::join_paths([
            self.bin.clone(),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ])
        .expect("gate PATH");
        Command::new(env!("CARGO_BIN_EXE_ostrom"))
            .args(["gate", &format!("example-org/example-repo#{number}")])
            .env_clear()
            .env("HOME", self.root.path())
            .env("PATH", path)
            .env("OSTROM_HOME", &self.home)
            .env("CLAUDE_CONFIG_DIR", self.root.path())
            .env("OSTROM_TEST_GATE_MODE", mode)
            .env("OSTROM_TEST_GATE_HEAD", head)
            .current_dir(self.root.path())
            .output()
            .expect("run gate")
    }

    fn write_config(&self, source: &str) {
        fs::write(self.home.join("gate.yaml"), source).expect("replace gate config");
    }

    fn materialize_current(&self, source: &str) -> String {
        let manifest = PolicyManifest::from_yaml(source).expect("current manifest");
        let digest = policy_manifest_digest(&manifest).expect("current manifest digest");
        let version = self.home.join("versions").join(&digest);
        fs::create_dir_all(&version).expect("create current version");
        fs::write(
            version.join("ostrom.yaml"),
            manifest.to_yaml().expect("canonical current manifest"),
        )
        .expect("write current manifest");
        symlink(
            Path::new("versions").join(&digest),
            self.home.join("current"),
        )
        .expect("point current manifest");
        digest
    }
}

fn executable(path: &Path, body: &str) {
    fs::write(path, format!("#!/usr/bin/env bash\nset -eu\n{body}\n")).expect("write gh stub");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod gh stub");
}

fn output_text(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("gate output UTF-8")
}

#[test]
fn unknown_missing_and_malformed_mergeability_are_inconclusive() {
    for mode in [
        "unknown-mergeable",
        "missing-mergeable",
        "malformed-mergeable",
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(mode, 7, "aaaaaaaaaaaaaaaa");
        assert_eq!(output.status.code(), Some(2), "{mode}");
        let text = output_text(&output);
        assert!(text.starts_with("verdict: inconclusive"), "{mode}: {text}");
        assert!(
            text.contains("condition mergeable: inconclusive"),
            "{mode}: {text}"
        );
    }
}

#[test]
fn current_manifest_source_is_reported_without_changing_a_passing_verdict() {
    let fixture = Fixture::new();
    let digest = fixture.materialize_current(
        r#"
manifest_version: 1
checks:
  verify: {uses: gh/check-run, with: {name: verify-linux}}
grants:
  merge:
    repositories: example-org/example-repo
    requires: verify
"#,
    );
    let output = fixture.run("pass", 7, "aaaaaaaaaaaaaaaa");
    assert_eq!(output.status.code(), Some(0));
    assert!(output_text(&output).starts_with("verdict: pass"));
    assert_eq!(
        String::from_utf8(output.stderr).expect("gate stderr"),
        format!("mandate gate: policy source=manifest digest={digest}\n")
    );
}

#[test]
fn repository_absent_from_current_manifest_is_named_by_run_gate() {
    let fixture = Fixture::new();
    let digest = fixture.materialize_current(
        "manifest_version: 1\ngrants:\n  other: {repositories: example-org/other}\n",
    );
    let output = fixture.run("pass", 7, "aaaaaaaaaaaaaaaa");
    assert_eq!(output.status.code(), Some(2));
    let stdout = output_text(&output);
    assert!(stdout.starts_with("verdict: inconclusive"), "{stdout}");
    assert!(
        stdout.contains("composed manifest has no project entry for example-org/example-repo"),
        "{stdout}"
    );
    assert_eq!(
        String::from_utf8(output.stderr).expect("gate stderr"),
        format!("mandate gate: policy source=manifest digest={digest}\n")
    );
}

#[test]
fn shipped_defaults_bounce_the_repository_manifest_even_when_user_config_omits_it() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.home.join("gate.yaml")).expect("remove user gate config");
    let output = fixture.run("policy-path", 6, "6666666666666666");
    assert_eq!(output.status.code(), Some(1));
    let text = output_text(&output);
    assert!(text.contains("condition bounce_selectors: fail"), "{text}");
    assert!(text.contains("path:ostrom.yaml"), "{text}");
}

#[test]
fn fly_spend_is_diff_sensitive_and_fails_closed_when_unobservable() {
    let fixture = Fixture::new();
    let output = fixture.run("fly-env", 8, "8888888888888888");
    assert!(output.status.success());
    assert!(output_text(&output).contains("condition bounce_selectors: pass"));

    for (number, mode) in [
        (9, "fly-machine"),
        (10, "fly-count"),
        (11, "fly-region"),
        (12, "fly-scaling"),
    ] {
        let output = fixture.run(mode, number, &format!("{number:016}"));
        assert_eq!(output.status.code(), Some(1), "{mode}");
        let text = output_text(&output);
        assert!(
            text.contains("condition bounce_selectors: fail"),
            "{mode}: {text}"
        );
        assert!(text.contains("substance:fly-spend"), "{mode}: {text}");
    }

    let output = fixture.run("diff-error", 13, "1313131313131313");
    assert_eq!(output.status.code(), Some(2));
    let text = output_text(&output);
    assert!(text.contains("condition bounce_selectors: inconclusive"));
    assert!(text.contains("placeholder diff unavailable"));

    fixture.write_config(
        "provider: file\nbounce_all: []\nprojects:\n  - repo: example-org/example-repo\n    required_checks: []\n    bounce: [substance:placeholder-unknown]\n    reserved: []\n",
    );
    let output = fixture.run("pass", 14, "1414141414141414");
    assert_eq!(output.status.code(), Some(2));
    assert!(output_text(&output).contains("unknown substance predicate: placeholder-unknown"));
}

#[test]
fn review_thread_details_do_not_turn_replies_or_self_resolution_into_passes() {
    for (mode, detail) in [
        (
            "thread-author",
            [
                "\"unresolved\":0",
                "\"answered\":0",
                "\"unanswered\":0",
                "\"resolved_by_pr_author\":1",
            ],
        ),
        (
            "thread-unanswered",
            [
                "\"unresolved\":1",
                "\"answered\":0",
                "\"unanswered\":1",
                "\"resolved_by_pr_author\":0",
            ],
        ),
        (
            "thread-answered",
            [
                "\"unresolved\":1",
                "\"answered\":1",
                "\"unanswered\":0",
                "\"resolved_by_pr_author\":0",
            ],
        ),
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(mode, 15, "1515151515151515");
        assert_eq!(output.status.code(), Some(1), "{mode}");
        let text = output_text(&output);
        assert!(text.contains("condition review_threads: fail"));
        for expected in detail {
            assert!(text.contains(expected), "{mode} missing {expected}: {text}");
        }
    }
}

#[test]
fn already_judged_is_keyed_by_pull_request_and_head_sha() {
    let fixture = Fixture::new();
    let first = output_text(&fixture.run("pass", 16, "aaaaaaaaaaaaaaaa"));
    let repeated = output_text(&fixture.run("pass", 16, "aaaaaaaaaaaaaaaa"));
    let advanced = output_text(&fixture.run("pass", 16, "bbbbbbbbbbbbbbbb"));
    assert!(
        first
            .lines()
            .next()
            .unwrap()
            .ends_with("already_judged=not-judged")
    );
    assert!(
        repeated
            .lines()
            .next()
            .unwrap()
            .ends_with("already_judged=judged")
    );
    assert!(
        advanced
            .lines()
            .next()
            .unwrap()
            .ends_with("already_judged=not-judged")
    );
}

#[test]
fn unreadable_check_runs_only_make_required_checks_inconclusive() {
    let fixture = Fixture::new();
    let output = fixture.run("check-runs-403", 20, "2020202020202020");
    assert_eq!(output.status.code(), Some(2));
    let text = output_text(&output);
    assert!(text.contains("head_sha=2020202020202020"), "{text}");
    assert!(text.contains("condition mergeable: pass"), "{text}");
    assert!(text.contains("condition draft: pass"), "{text}");
    assert!(
        text.contains("condition required_checks: inconclusive"),
        "{text}"
    );
    assert!(
        text.contains("Resource not accessible by integration"),
        "{text}"
    );
    for name in [
        "mergeable",
        "draft",
        "review_threads",
        "bounce_selectors",
        "reserved_refs",
    ] {
        assert!(
            !text.contains(&format!("condition {name}: inconclusive")),
            "{text}"
        );
    }
}

#[test]
fn rest_check_runs_distinguish_pass_failure_pending_and_absence() {
    for (mode, code, result, detail) in [
        ("pass", 0, "pass", "\"state\":\"SUCCESS\""),
        ("check-failure", 1, "fail", "verify-linux"),
        ("check-running", 2, "inconclusive", "\"result\":\"pending\""),
        ("no-check-runs", 1, "fail", "\"matches\":[]"),
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(mode, 23, "2323232323232323");
        assert_eq!(output.status.code(), Some(code), "{mode}");
        let text = output_text(&output);
        assert!(
            text.contains(&format!("condition required_checks: {result}")),
            "{mode}: {text}"
        );
        assert!(text.contains(detail), "{mode}: {text}");
        if mode == "check-running" {
            assert!(text.contains("verify-linux"), "{text}");
            assert!(!text.contains("condition required_checks: fail"), "{text}");
            assert!(!text.contains("condition required_checks: pass"), "{text}");
        }
    }
}

#[test]
fn unreadable_statuses_are_recorded_without_discarding_check_runs() {
    let fixture = Fixture::new();
    let output = fixture.run("status-403", 24, "2424242424242424");
    assert!(output.status.success());
    let text = output_text(&output);
    assert!(text.contains("condition required_checks: pass"), "{text}");
    assert!(text.contains("\"partial_read\""), "{text}");
    assert!(text.contains("commit status"), "{text}");
    assert!(text.contains("403"), "{text}");
}

#[test]
fn commit_status_contexts_share_the_required_check_evaluator() {
    let fixture = Fixture::new();
    let output = fixture.run("status-check", 25, "2525252525252525");
    assert!(output.status.success());
    let text = output_text(&output);
    assert!(text.contains("condition required_checks: pass"), "{text}");
    assert!(text.contains("verify-linux"), "{text}");
    assert!(text.contains("\"state\":\"SUCCESS\""), "{text}");
}

#[test]
fn sha_less_judgments_are_durable_delivery_memory_but_not_evidence() {
    let fixture = Fixture::new();
    let first = output_text(&fixture.run("metadata-403", 21, "ignored"));
    let repeated = output_text(&fixture.run("metadata-403", 21, "ignored"));
    assert!(
        first
            .lines()
            .next()
            .unwrap()
            .ends_with("already_judged=not-judged")
    );
    assert!(
        repeated
            .lines()
            .next()
            .unwrap()
            .ends_with("already_judged=judged")
    );

    let records = fs::read_to_string(fixture.home.join("gate.jsonl")).unwrap();
    let records = records
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|record| record["head_sha"].is_null()));
    assert!(records.iter().all(|record| record["evidence"] == false));
}

#[test]
fn changed_judgment_on_the_same_head_is_not_suppressed() {
    let fixture = Fixture::new();
    let first = output_text(&fixture.run("pass", 22, "2222222222222222"));
    let changed = output_text(&fixture.run("draft", 22, "2222222222222222"));
    let repeated = output_text(&fixture.run("draft", 22, "2222222222222222"));
    assert!(
        first
            .lines()
            .next()
            .unwrap()
            .ends_with("already_judged=not-judged")
    );
    assert!(
        changed
            .lines()
            .next()
            .unwrap()
            .ends_with("already_judged=not-judged")
    );
    assert!(
        repeated
            .lines()
            .next()
            .unwrap()
            .ends_with("already_judged=judged")
    );
}

#[test]
fn exceptions_are_condition_and_sha_scoped_for_failures_and_inconclusive_results() {
    let fixture = Fixture::new();
    fixture.write_config(
        "provider: file\nbounce_all: []\nprojects:\n  - repo: example-org/example-repo\n    required_checks: []\n    bounce: [label:risk:protected-surface]\n    reserved: []\n",
    );
    fs::write(
        fixture.home.join("exceptions.jsonl"),
        concat!(
            r#"{"ts":"2026-08-01T00:00:00Z","repo":"example-org/example-repo","pr":17,"head_sha":"aaaaaaaaaaaaaaaa","condition":"bounce_selectors","reason":"principal accepted placeholder surface"}"#,
            "\n",
        ),
    )
    .expect("write matching exception");
    for _ in 0..2 {
        let output = fixture.run("bounce", 17, "aaaaaaaaaaaaaaaa");
        assert!(output.status.success());
        let text = output_text(&output);
        assert!(text.contains("condition bounce_selectors: excused"));
        assert!(text.contains("principal accepted placeholder surface"));
    }
    assert_eq!(
        fixture.run("bounce", 17, "bbbbbbbbbbbbbbbb").status.code(),
        Some(1)
    );

    fs::write(
        fixture.home.join("exceptions.jsonl"),
        concat!(
            r#"{"ts":"2026-08-01T00:00:00Z","repo":"example-org/example-repo","pr":18,"head_sha":"cccccccccccccccc","condition":"draft","reason":"wrong condition"}"#,
            "\n",
        ),
    )
    .expect("write wrong-condition exception");
    assert_eq!(
        fixture.run("bounce", 18, "cccccccccccccccc").status.code(),
        Some(1)
    );

    fs::write(
        fixture.home.join("exceptions.jsonl"),
        concat!(
            r#"{"ts":"2026-08-01T00:00:00Z","repo":"example-org/example-repo","pr":19,"head_sha":"dddddddddddddddd","condition":"mergeable","reason":"principal accepted unavailable mergeability"}"#,
            "\n",
        ),
    )
    .expect("write inconclusive exception");
    let output = fixture.run("unknown-mergeable", 19, "dddddddddddddddd");
    assert!(output.status.success());
    assert!(output_text(&output).contains("condition mergeable: excused"));
}
