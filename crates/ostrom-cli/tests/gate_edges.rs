#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

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
    bounce: [title:*protected*, substance:fly-spend]
    reserved: []
"#,
        )
        .expect("write gate config");
        executable(
            &bin.join("gh"),
            r#"
mode=${OSTROM_TEST_GATE_MODE:-pass}
if [ "$1 $2" = "pr view" ]; then
  mergeable=MERGEABLE
  draft=false
  title='fix(core): safe placeholder change'
  check=SUCCESS
  case "$mode" in
    unknown-mergeable) mergeable=UNKNOWN ;;
    draft) draft=true ;;
    bounce) title='feat(core): protected placeholder change' ;;
    unknown-check) check=UNRECOGNIZED ;;
  esac
  jq -cn --argjson number "$3" --arg head "${OSTROM_TEST_GATE_HEAD:-aaaaaaaaaaaaaaaa}" \
    --arg mergeable "$mergeable" --argjson draft "$draft" --arg title "$title" --arg check "$check" '
    {number:$number,title:$title,author:{login:"builder-login"},headRefOid:$head,labels:[],
     statusCheckRollup:[{name:"verify-linux",status:"COMPLETED",conclusion:$check}],
     closingIssuesReferences:[],mergeable:$mergeable,isDraft:$draft}
    | if env.OSTROM_TEST_GATE_MODE == "missing-mergeable" then del(.mergeable)
      elif env.OSTROM_TEST_GATE_MODE == "malformed-mergeable" then .mergeable=7 else . end'
  exit 0
fi
if [ "$1 $2" = "pr diff" ]; then
  if printf '%s\n' "$*" | grep -q -- '--name-only'; then
    case "$mode" in fly-*|diff-error) printf '%s\n' deploy/fly.toml ;; *) printf '%s\n' src/placeholder.rs ;; esac
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
            .env("MANDATE_GATE_TIME", "2026-08-01T00:00:00Z")
            .env("MANDATE_NOW_EPOCH", "1785542400")
            .env("MANDATE_TODAY", "2026-08-01")
            .env("MANDATE_SWEEP_TIME", "2026-08-01T00:00:00Z")
            .current_dir(self.root.path())
            .output()
            .expect("run gate")
    }

    fn write_config(&self, source: &str) {
        fs::write(self.home.join("gate.yaml"), source).expect("replace gate config");
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
            .ends_with("already_judged=false")
    );
    assert!(
        repeated
            .lines()
            .next()
            .unwrap()
            .ends_with("already_judged=true")
    );
    assert!(
        advanced
            .lines()
            .next()
            .unwrap()
            .ends_with("already_judged=false")
    );
}

#[test]
fn exceptions_are_condition_and_sha_scoped_for_failures_and_inconclusive_results() {
    let fixture = Fixture::new();
    fixture.write_config(
        "provider: file\nbounce_all: []\nprojects:\n  - repo: example-org/example-repo\n    required_checks: []\n    bounce: [title:*protected*]\n    reserved: []\n",
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
