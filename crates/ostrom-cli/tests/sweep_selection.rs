#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;
mod support;
use support::tree::{assert_unchanged, snapshot};

const ALPHA: &str = "placeholder-org/alpha";
const BETA: &str = "placeholder-org/beta";
const FIRST: &str = "2026-08-02T00:00:00Z";
const NEXT: &str = "2026-08-03T00:00:00Z";
const ROSTER: &str = include_str!("fixtures/sweep-selection/mandates.yaml");
const RESPONSES: &str = include_str!("fixtures/sweep-selection/responses.json");

struct Fixture {
    root: TempDir,
    home: std::path::PathBuf,
    responses: std::path::PathBuf,
}
impl Fixture {
    fn new(empty: bool) -> Self {
        let root = TempDir::new().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(home.join("nested/empty")).unwrap();
        fs::write(home.join("nested/receipt"), b"preserved\0bytes\n").unwrap();
        std::os::unix::fs::symlink("receipt", home.join("nested/link")).unwrap();
        fs::write(home.join("mandates.yaml"), ROSTER).unwrap();
        fs::write(
            home.join("gate.jsonl"),
            include_bytes!("fixtures/sweep-selection/full.gate.jsonl"),
        )
        .unwrap();
        let responses = root.path().join("responses.json");
        let fixture = Self {
            root,
            home,
            responses,
        };
        let mut value: Value = serde_json::from_str(RESPONSES).unwrap();
        if empty {
            for repo in value["repositories"].as_array_mut().unwrap() {
                repo["issues"] = json!([]);
            }
        }
        fixture.write(&value);
        fixture
    }
    fn write(&self, value: &Value) {
        fs::write(&self.responses, serde_json::to_vec(value).unwrap()).unwrap();
    }
    fn responses(&self) -> Value {
        serde_json::from_slice(&fs::read(&self.responses).unwrap()).unwrap()
    }
    fn run(&self, args: &[&str], time: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_ostrom"))
            .env_clear()
            .env("OSTROM_HOME", &self.home)
            .current_dir(self.root.path())
            .args(["sweep", "--fixture"])
            .arg(&self.responses)
            .args(["--started-at", time])
            .args(args)
            .output()
            .unwrap()
    }
    fn sweep(&self, args: &[&str], time: &str) -> String {
        success(self.run(args, time))
    }
    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.home.join("state.json")).unwrap()).unwrap()
    }
    fn conditional(&self) {
        let mut value = self.responses();
        for repo in value["repositories"].as_array_mut().unwrap() {
            repo["issue_not_modified"] = json!(true);
        }
        self.write(&value);
    }
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn repo_rows(path: &Path, repo: &str) -> Vec<Vec<u8>> {
    fs::read(path)
        .unwrap()
        .split_inclusive(|byte| *byte == b'\n')
        .filter(|line| serde_json::from_slice::<Value>(line).unwrap()["repo"] == repo)
        .map(<[u8]>::to_vec)
        .collect()
}
// Read the original JSON value span, retaining whitespace and key order.
fn repo_state_bytes(path: &Path, repo: &str) -> Vec<u8> {
    let text = fs::read_to_string(path).unwrap();
    let key = format!("\"{repo}\": ");
    let start = text.find(&key).unwrap() + key.len();
    let mut values = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
    values.next().unwrap().unwrap();
    text.as_bytes()[start..start + values.byte_offset()].to_vec()
}

#[test]
fn full_sweep_matches_base_generation_bytes_and_stdout() {
    let fixture = Fixture::new(false);
    assert_eq!(
        fixture.sweep(&[], FIRST),
        include_str!("fixtures/sweep-selection/full.stdout")
    );
    for (file, expected) in [
        (
            "queue.jsonl",
            include_bytes!("fixtures/sweep-selection/full.queue.jsonl").as_slice(),
        ),
        (
            "state.json",
            include_bytes!("fixtures/sweep-selection/full.state.json").as_slice(),
        ),
        (
            "gate.jsonl",
            include_bytes!("fixtures/sweep-selection/full.gate.jsonl").as_slice(),
        ),
    ] {
        assert_eq!(
            fs::read(fixture.home.join(file)).unwrap(),
            expected,
            "full generation {file} drifted"
        );
    }
}

#[test]
fn selected_sweep_carries_records_and_observes_only_read_repositories() {
    let fixture = Fixture::new(false);
    fixture.sweep(&[], FIRST);
    let queue = repo_rows(&fixture.home.join("queue.jsonl"), BETA);
    assert!(!queue.is_empty());
    let state = repo_state_bytes(&fixture.home.join("state.json"), BETA);
    let gates = fs::read(fixture.home.join("gate.jsonl")).unwrap();
    let mut value = fixture.responses();
    value["repositories"][0]["issues"][0]["title"] = json!("alpha changed");
    value["repositories"][0]["issues"][0]["updated_at"] = json!(NEXT);
    value["repositories"][1]["issues"][0]["title"] = json!("beta must not be read");
    value["repositories"][0]["merged_prs"] = json!([{
        "number": 2, "title": "merged work", "state": "MERGED", "createdAt": FIRST, "updatedAt": NEXT,
        "mergedAt": NEXT, "author": {"login": "placeholder-person"}, "mergedBy": {"login": "placeholder-person"},
        "headRefOid": "abc", "headRefName": "placeholder-branch", "files": [], "closingIssuesReferences": []
    }]);
    fixture.write(&value);
    let stdout = fixture.sweep(&["--repositories", ALPHA, "--mode", "full"], NEXT);
    assert!(
        stdout.ends_with("; 1 repositories read; 1 carried forward\n"),
        "{stdout}"
    );
    assert_eq!(
        fixture.state()["repos"][ALPHA]["records"][format!("{ALPHA}#1")]["title"],
        "alpha changed"
    );
    assert!(
        repo_rows(&fixture.home.join("queue.jsonl"), ALPHA)
            .iter()
            .any(|line| serde_json::from_slice::<Value>(line).unwrap()["title"] == "alpha changed")
    );
    assert_eq!(
        queue,
        repo_rows(&fixture.home.join("queue.jsonl"), BETA),
        "carried queue bytes changed"
    );
    assert_eq!(
        state,
        repo_state_bytes(&fixture.home.join("state.json"), BETA),
        "carried state bytes changed"
    );
    assert_eq!(
        gates,
        fs::read(fixture.home.join("gate.jsonl")).unwrap(),
        "gate bytes changed"
    );
    assert_eq!(
        fixture.state()["velocity"]["observed_days"]["2026-08-03"],
        json!([ALPHA]),
        "carried repository fabricated an observation"
    );
    assert_eq!(
        fixture.state()["last_full_reconciliation"],
        FIRST,
        "partial acquisition must not reset whole-roster reconciliation"
    );
    let trace = fs::read(fixture.home.join("sprint.jsonl")).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&trace)
            .matches("\"pr-merged\"")
            .count(),
        1
    );
    fixture.sweep(&["--repositories", ALPHA, "--mode", "incremental"], NEXT);
    assert_eq!(
        trace,
        fs::read(fixture.home.join("sprint.jsonl")).unwrap(),
        "merge facts duplicated"
    );
    assert_eq!(
        fs::read(fixture.home.join("previous/queue.jsonl")).unwrap(),
        fs::read(fixture.home.join("queue.jsonl")).unwrap()
    );
}

#[test]
fn selected_non_roster_refusal_leaves_whole_home_untouched() {
    for prior in [false, true] {
        let fixture = Fixture::new(false);
        if prior {
            fixture.sweep(&[], FIRST);
        }
        let before = snapshot(&fixture.home);
        let output = fixture.run(&["--repositories", "placeholder-org/missing"], NEXT);
        assert!(!output.status.success(), "unknown repository succeeded");
        assert!(String::from_utf8_lossy(&output.stderr).contains("placeholder-org/missing"));
        assert_unchanged(&before, &snapshot(&fixture.home));
    }
}

#[test]
fn detect_round_trip_uses_only_real_sweep_etags() {
    let fixture = Fixture::new(true);
    let changed = format!("changed: {ALPHA}\nchanged: {BETA}\ndetect: 2 changed of 2\n");
    assert_eq!(fixture.sweep(&["--detect"], FIRST), changed);
    assert_eq!(
        fixture.sweep(&["--detect"], FIRST),
        changed,
        "detect must not establish its own baseline"
    );
    fixture.sweep(&[], FIRST);
    fixture.conditional();
    assert_eq!(
        fixture.sweep(&["--detect"], NEXT),
        format!("unchanged: {ALPHA}\nunchanged: {BETA}\ndetect: 0 changed of 2\n")
    );
    let mut value = fixture.responses();
    value["repositories"][1]["issue_etag"] = json!("changed");
    fixture.write(&value);
    assert_eq!(
        fixture.sweep(&["--detect"], NEXT),
        format!("unchanged: {ALPHA}\nchanged: {BETA}\ndetect: 1 changed of 2\n")
    );
}

#[test]
fn detect_writes_nothing_under_whole_home() {
    let fixture = Fixture::new(true);
    for prior in [false, true] {
        if prior {
            fixture.sweep(&[], FIRST);
            fixture.conditional();
        }
        let before = snapshot(&fixture.home);
        fixture.sweep(&["--detect"], NEXT);
        assert_unchanged(&before, &snapshot(&fixture.home));
    }
}

#[test]
fn detect_reports_pr_only_change_with_unchanged_issues() {
    let fixture = Fixture::new(true);
    fixture.sweep(&[], FIRST);
    fixture.conditional();
    let mut value = fixture.responses();
    value["repositories"][0]["open_prs"] =
        json!([{"number": 2, "updatedAt": NEXT, "state": "OPEN"}]);
    fixture.write(&value);
    assert_eq!(
        fixture.sweep(&["--detect"], NEXT),
        format!("changed: {ALPHA}\nunchanged: {BETA}\ndetect: 1 changed of 2\n")
    );
}

#[test]
fn detect_refuses_unknown_or_unavailable_generations_without_writes() {
    let fixture = Fixture::new(true);
    for id in ["../outside", "previous", "current"] {
        let before = snapshot(&fixture.home);
        assert!(
            !fixture
                .run(&["--detect", "--since", id], NEXT)
                .status
                .success()
        );
        assert_unchanged(&before, &snapshot(&fixture.home));
    }
    fixture.sweep(&[], FIRST);
    fixture.sweep(&[], NEXT);
    fixture.conditional();
    assert_eq!(
        fixture.sweep(&["--detect", "--since", "previous"], NEXT),
        format!("unchanged: {ALPHA}\nunchanged: {BETA}\ndetect: 0 changed of 2\n")
    );
}

fn validate_sweep_policy(value: &str) -> Output {
    let root = TempDir::new().unwrap();
    let manifest = root.path().join("policy.yaml");
    fs::write(&manifest, format!("manifest_version: 1\nsweep: {value}\n")).unwrap();
    Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .env_clear()
        .env("OSTROM_HOME", root.path())
        .current_dir(root.path())
        .args(["validate", "--unsigned"])
        .arg(&manifest)
        .output()
        .unwrap()
}

#[test]
fn sweep_schema_accepts_defaults_and_shared_duration_grammar() {
    let manifest =
        ostrom_core::PolicyManifest::from_yaml("manifest_version: 1\nsweep: {}\n").unwrap();
    let sweep = manifest.sweep.unwrap();
    assert_eq!(sweep.max_age, "30m");
    assert_eq!(sweep.detect_every, "5m");
    success(validate_sweep_policy("{}"));
    for value in ["1s", "05m", "2h", "3d", "4w"] {
        success(validate_sweep_policy(&format!(
            "{{max_age: '{value}', detect_every: '{value}'}}"
        )));
    }
    let absent = ostrom_core::PolicyManifest::from_yaml("manifest_version: 1\n").unwrap();
    assert!(absent.sweep.is_none());
    assert!(!absent.to_yaml().unwrap().contains("sweep:"));
}

#[test]
fn sweep_schema_refuses_non_positive_duration() {
    for field in ["max_age", "detect_every"] {
        for value in ["0s", "0m", "-1h"] {
            let output = validate_sweep_policy(&format!("{{{field}: '{value}'}}"));
            assert!(!output.status.success(), "sweep.{field} accepted {value}");
            assert!(String::from_utf8_lossy(&output.stderr).contains(&format!("sweep.{field}")));
        }
    }
}

#[test]
fn sweep_schema_refuses_unparseable_duration() {
    for field in ["max_age", "detect_every"] {
        for value in [
            "",
            "30",
            "1ms",
            "1.5h",
            " 1h",
            "1h ",
            "+1h",
            "18446744073709551615w",
            "later",
        ] {
            let output = validate_sweep_policy(&format!("{{{field}: '{value}'}}"));
            assert!(!output.status.success(), "sweep.{field} accepted {value}");
            assert!(String::from_utf8_lossy(&output.stderr).contains(&format!("sweep.{field}")));
        }
    }
}

#[test]
fn detect_cannot_certify_branch_ci_or_existing_pr_evidence() {
    let fixture = Fixture::new(true);
    fixture.sweep(&[], FIRST);
    fixture.conditional();
    for (field, evidence) in [
        ("default_branch", json!("main")),
        ("branches", json!([{"name":"topic"}])),
        ("ci_runs", json!([{"status":"completed"}])),
        ("branch_read_degraded", json!(true)),
        ("warnings", json!(["unobserved"])),
    ] {
        let mut value = fixture.responses();
        value["repositories"][0][field] = evidence;
        fixture.write(&value);
        assert_eq!(
            fixture.sweep(&["--detect"], NEXT),
            format!("changed: {ALPHA}\nunchanged: {BETA}\ndetect: 1 changed of 2\n")
        );
        value["repositories"][0]
            .as_object_mut()
            .unwrap()
            .remove(field);
        fixture.write(&value);
    }
}

#[test]
fn live_detect_checks_conditional_issues_prs_and_branch_without_writes() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new(true);
    fixture.sweep(&[], FIRST);
    let bin = fixture.root.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let gh = bin.join("gh");
    fs::write(
        &gh,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$PROBE_LOG"
case "$*" in
  'api '*If-None-Match*) printf 'HTTP/2 304\r\n\r\n' ;;
  'api '*'/issues?'*) printf 'HTTP/2 200\r\n\r\n[]' ;;
  'pr list --repo placeholder-org/alpha '*) /bin/cat "$PR_RESPONSE" ;;
  'pr list --repo placeholder-org/beta '*) printf '[]' ;;
  'repo view '*) printf '{"defaultBranchRef":null}' ;;
  *) exit 1 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let log = fixture.root.path().join("probes");
    let prs = fixture.root.path().join("prs.json");
    fs::write(&prs, "[]").unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_ostrom"))
            .env_clear()
            .env("OSTROM_HOME", &fixture.home)
            .env("PATH", &bin)
            .env("PROBE_LOG", &log)
            .env("PR_RESPONSE", &prs)
            .current_dir(fixture.root.path())
            .args(["sweep", "--detect"])
            .output()
            .unwrap()
    };
    let before = snapshot(&fixture.home);
    assert_eq!(
        success(run()),
        format!("unchanged: {ALPHA}\nunchanged: {BETA}\ndetect: 0 changed of 2\n")
    );
    fs::write(
        &prs,
        format!("[{{\"number\":2,\"updatedAt\":\"{NEXT}\",\"state\":\"OPEN\"}}]"),
    )
    .unwrap();
    assert_eq!(
        success(run()),
        format!("changed: {ALPHA}\nunchanged: {BETA}\ndetect: 1 changed of 2\n")
    );
    assert_unchanged(&before, &snapshot(&fixture.home));
    let log = fs::read_to_string(log).unwrap();
    for name in ["alpha", "beta"] {
        assert!(
            log.contains(&format!("If-None-Match: \"{name}-issues\"")),
            "stored ETag was not sent: {log}"
        );
        assert!(log.contains(&format!("pr list --repo placeholder-org/{name} --state all --limit 200 --json number,updatedAt,state")));
        assert!(log.contains(&format!(
            "repo view placeholder-org/{name} --json defaultBranchRef"
        )));
    }
    assert!(log.contains("state=all"));
    assert!(log.contains("since=2026-08-02T00:00:00Z"));
}

#[test]
fn carried_repository_keeps_holds_and_ranking_faults_when_policy_is_removed() {
    let fixture = Fixture::new(false);
    fixture.sweep(&[], FIRST);
    let mut state = fixture.state();
    let hold = json!({"id":format!("{BETA}#2"), "repo":BETA, "title":"held work", "first_held": FIRST, "verdict":"HOLD", "stalled":true});
    state["policy_holds"] =
        json!({format!("{BETA}#2"):hold.clone(), format!("{ALPHA}#2"): {"repo": ALPHA}});
    state["stalled_holds"] = json!([hold.clone()]);
    state["work_ranking_faults"] = json!([format!("{BETA}#404")]);
    fs::write(
        fixture.home.join("state.json"),
        serde_json::to_vec_pretty(&state).unwrap(),
    )
    .unwrap();
    fixture.sweep(&["--repositories", ALPHA], NEXT);
    let state = fixture.state();
    assert_eq!(
        state["policy_holds"],
        json!({format!("{BETA}#2"):hold.clone()}),
        "carried hold was retired without a read"
    );
    assert_eq!(state["stalled_holds"], json!([hold]));
    assert_eq!(
        state["work_ranking_faults"],
        json!([format!("{BETA}#404")]),
        "carried ranking fault was erased"
    );
}
