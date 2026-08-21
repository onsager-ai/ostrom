use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use chrono::{Duration, SecondsFormat};
use serde_json::Value;
use tempfile::tempdir;

struct Case {
    name: &'static str,
    mode: &'static str,
    number: u64,
    head: &'static str,
}

const CASES: &[Case] = &[
    Case {
        name: "pass",
        mode: "pass",
        number: 7,
        head: "aaaaaaaaaaaaaaaa",
    },
    Case {
        name: "conflicting",
        mode: "conflicting",
        number: 13,
        head: "1313131313131313",
    },
    Case {
        name: "draft",
        mode: "draft",
        number: 14,
        head: "1414141414141414",
    },
    Case {
        name: "check-failure",
        mode: "check-failure",
        number: 15,
        head: "1515151515151515",
    },
    Case {
        name: "thread-failure",
        mode: "thread-failure",
        number: 16,
        head: "1616161616161616",
    },
    Case {
        name: "bounce",
        mode: "bounce",
        number: 17,
        head: "1717171717171717",
    },
    Case {
        name: "reserved",
        mode: "reserved",
        number: 99,
        head: "9999999999999998",
    },
    Case {
        name: "excused",
        mode: "bounce",
        number: 9,
        head: "9999999999999999",
    },
];

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gate")
}

fn fixture_path() -> OsString {
    let fixture_bin = fixture_root().join("bin");
    let mut paths = vec![fixture_bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(paths).expect("join fixture PATH")
}

#[test]
fn rust_gate_matches_recorded_shell_corpus_apart_from_the_injected_clock() {
    let fixture = fixture_root();
    for case in CASES {
        let root = tempdir().expect("temporary gate fixture");
        let home = root.path().join("home");
        let repo = root.path().join("repo");
        fs::create_dir_all(&home).expect("create synthetic gate home");
        fs::create_dir_all(&repo).expect("create synthetic repository");
        fs::copy(fixture.join("config/gate.yaml"), home.join("gate.yaml"))
            .expect("install synthetic gate config");
        if case.name == "excused" {
            fs::copy(
                fixture.join("exceptions.jsonl"),
                home.join("exceptions.jsonl"),
            )
            .expect("install synthetic exception log");
        }
        let command_log = root.path().join("commands");
        let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
            .args(["gate", &format!("placeholder-org/alpha#{}", case.number)])
            .env("PATH", fixture_path())
            .env("OSTROM_HOME", &home)
            .env("GATE_FIXTURE_MODE", case.mode)
            .env("GATE_FIXTURE_HEAD", case.head)
            .env("GATE_COMMAND_LOG", &command_log)
            .current_dir(&repo)
            .output()
            .expect("run Rust gate");

        let expected = fixture.join("expected");
        let expected_status = fs::read_to_string(expected.join(format!("{}.status", case.name)))
            .expect("read recorded status")
            .trim()
            .parse::<i32>()
            .expect("recorded status is numeric");
        assert_eq!(
            output.status.code(),
            Some(expected_status),
            "{} status",
            case.name
        );
        assert_eq!(
            output.stdout,
            fs::read(expected.join(format!("{}.stdout", case.name))).expect("read recorded stdout"),
            "{} stdout",
            case.name
        );
        assert_eq!(
            output.stderr,
            fs::read(expected.join(format!("{}.stderr", case.name))).expect("read recorded stderr"),
            "{} stderr",
            case.name
        );
        let mut actual_record: Value = serde_json::from_slice(
            &fs::read(home.join("gate.jsonl")).expect("read Rust verdict log"),
        )
        .expect("parse Rust verdict");
        let expected_bytes = fs::read(expected.join(format!("{}.gate.jsonl", case.name)))
            .expect("read recorded shell verdict");
        let expected_record: Value =
            serde_json::from_slice(&expected_bytes).expect("parse recorded shell verdict");
        actual_record["ts"] = expected_record["ts"].clone();
        let mut normalized = serde_json::to_vec(&actual_record).expect("serialize verdict");
        normalized.push(b'\n');
        assert_eq!(normalized, expected_bytes, "{} gate log", case.name);
        if let Some(condition_name) = match case.name {
            "conflicting" => Some("mergeable"),
            "draft" => Some("draft"),
            "check-failure" => Some("required_checks"),
            "thread-failure" => Some("review_threads"),
            "bounce" => Some("bounce_selectors"),
            "reserved" => Some("reserved_refs"),
            _ => None,
        } {
            let record: Value = serde_json::from_slice(
                &fs::read(home.join("gate.jsonl")).expect("read refusal record"),
            )
            .expect("parse refusal record");
            assert!(record["conditions"].as_array().is_some_and(|conditions| {
                conditions.iter().any(|condition| {
                    condition["name"] == condition_name && condition["result"] == "fail"
                })
            }));
        }

        let commands = fs::read_to_string(command_log).expect("read GitHub boundary log");
        assert!(
            commands
                .lines()
                .all(|line| matches!(line, "pr view" | "pr diff" | "api graphql"))
        );
    }
}

#[test]
fn malformed_target_is_named_usage_error_and_writes_nothing() {
    let fixture = fixture_root();
    let home = tempdir().expect("temporary OSTROM_HOME");
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["gate", "not-a-pull-request"])
        .env("OSTROM_HOME", home.path())
        .output()
        .expect("run malformed gate input");
    assert_eq!(output.status.code(), Some(64));
    assert_eq!(
        output.stdout,
        fs::read(fixture.join("expected/malformed.stdout")).expect("recorded malformed stdout")
    );
    assert_eq!(
        output.stderr,
        fs::read(fixture.join("expected/malformed.stderr")).expect("recorded malformed stderr")
    );
    assert!(!home.path().join("gate.jsonl").exists());
}

#[test]
fn unresolved_head_is_not_written_as_a_verdict() {
    let fixture = fixture_root();
    let root = tempdir().expect("temporary gate fixture");
    fs::copy(
        fixture.join("config/gate.yaml"),
        root.path().join("gate.yaml"),
    )
    .expect("install gate config");
    let command_log = root.path().join("commands");
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["gate", "placeholder-org/alpha#7"])
        .env("PATH", fixture_path())
        .env("OSTROM_HOME", root.path())
        .env("GATE_FIXTURE_MODE", "unresolvable")
        .env("GATE_COMMAND_LOG", &command_log)
        .current_dir(root.path())
        .output()
        .expect("run unresolved gate input");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("verdict: inconclusive "));
    assert!(!root.path().join("gate.jsonl").exists());
}

#[test]
fn configuration_cannot_grant_an_exception() {
    let root = tempdir().expect("temporary gate fixture");
    fs::write(
        root.path().join("gate.yaml"),
        r#"provider: file
bounce_all: []
projects:
  - repo: placeholder-org/alpha
    required_checks: []
    bounce: [title:*protected*]
    reserved: []
    exceptions:
      bounce_selectors: principal accepted placeholder surface
"#,
    )
    .expect("write rejected exception-shaped policy");
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["gate", "placeholder-org/alpha#7"])
        .env("PATH", fixture_path())
        .env("OSTROM_HOME", root.path())
        .env("GATE_FIXTURE_MODE", "bounce")
        .env("GATE_FIXTURE_HEAD", "aaaaaaaaaaaaaaaa")
        .current_dir(root.path())
        .output()
        .expect("run gate with exception-shaped config");
    assert_eq!(output.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(": excused "));
}

#[test]
fn sweep_reads_the_gate_writer_by_repository_pr_and_head_sha() {
    let fixture = fixture_root();
    let root = tempdir().expect("temporary end-to-end fixture");
    fs::copy(
        fixture.join("config/gate.yaml"),
        root.path().join("gate.yaml"),
    )
    .expect("install gate config");
    fs::write(
        root.path().join("mandates.yaml"),
        r#"provider: file
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
"#,
    )
    .expect("write synthetic roster");
    let gate = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["gate", "placeholder-org/alpha#7"])
        .env("PATH", fixture_path())
        .env("OSTROM_HOME", root.path())
        .env("GATE_FIXTURE_MODE", "pass")
        .env("GATE_FIXTURE_HEAD", "aaaaaaaaaaaaaaaa")
        .current_dir(root.path())
        .output()
        .expect("record gate verdict");
    assert!(gate.status.success());

    let gate_record: Value = serde_json::from_slice(
        &fs::read(root.path().join("gate.jsonl")).expect("read gate-produced evidence"),
    )
    .expect("parse gate-produced evidence");
    let gate_time =
        chrono::DateTime::parse_from_rfc3339(gate_record["ts"].as_str().expect("gate timestamp"))
            .expect("valid gate timestamp");
    let merged_at = (gate_time + Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let first_sweep = (gate_time + Duration::hours(2)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let second_sweep = (gate_time + Duration::hours(3)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let sweep_fixture = root.path().join("sweep.json");
    fs::write(
        &sweep_fixture,
        serde_json::json!({
            "repositories": [{
                "repo": "placeholder-org/alpha",
                "issues": [],
                "open_prs": [],
                "merged_prs": [{
                    "number": 7,
                    "title": "fix(core): safe placeholder change",
                    "author": {"login": "placeholder-builder", "isBot": false},
                    "createdAt": gate_time.to_rfc3339_opts(SecondsFormat::Secs, true),
                    "mergedAt": merged_at,
                    "headRefOid": "aaaaaaaaaaaaaaaa",
                    "closingIssuesReferences": [{"number": 42}]
                }],
                "branches": [],
                "ci_runs": []
            }]
        })
        .to_string(),
    )
    .expect("write synthetic sweep acquisition");
    let sweep = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "sweep",
            "--fixture",
            sweep_fixture.to_str().expect("fixture path is UTF-8"),
            "--started-at",
            &first_sweep,
        ])
        .env("OSTROM_HOME", root.path())
        .current_dir(root.path())
        .output()
        .expect("run end-to-end sweep");
    assert!(
        sweep.status.success(),
        "sweep stderr: {}",
        String::from_utf8_lossy(&sweep.stderr)
    );
    let queue = fs::read_to_string(root.path().join("queue.jsonl")).unwrap_or_default();
    let rows = queue
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("parse synthetic queue row"))
        .collect::<Vec<_>>();
    assert!(rows.iter().all(|row| row["kind"] != "merge-gate-fault"));

    let gate_path = root.path().join("gate.jsonl");
    let mut record: Value =
        serde_json::from_slice(&fs::read(&gate_path).expect("read gate-produced evidence"))
            .expect("parse gate-produced evidence");
    record["head_sha"] = Value::String("bbbbbbbbbbbbbbbb".to_owned());
    fs::write(&gate_path, format!("{record}\n")).expect("write mismatched synthetic evidence");
    let mismatch = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "sweep",
            "--fixture",
            sweep_fixture.to_str().expect("fixture path is UTF-8"),
            "--started-at",
            &second_sweep,
        ])
        .env("OSTROM_HOME", root.path())
        .current_dir(root.path())
        .output()
        .expect("run mismatched end-to-end sweep");
    assert!(mismatch.status.success());
    let queue = fs::read_to_string(root.path().join("queue.jsonl"))
        .expect("read mismatched merge-gate queue");
    assert!(queue.lines().any(|line| {
        serde_json::from_str::<Value>(line).is_ok_and(|row| row["kind"] == "merge-gate-fault")
    }));
}
