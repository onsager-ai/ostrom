#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use tempfile::{TempDir, tempdir};

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/leaves/state-writing"
);

#[test]
fn queue_verbs_match_recorded_shell_bytes_and_unknown_ids_refuse() {
    for (verb, expected) in [
        ("list", "queue.list.expected.jsonl"),
        ("lint", "queue.lint.expected.txt"),
        ("approve", "queue.approve.expected.txt"),
        ("reject", "queue.reject.expected.jsonl"),
        ("defer", "queue.defer.expected.jsonl"),
    ] {
        let home = queue_home();
        let mut command = ostrom(home.path());
        command.args(["queue", verb]);
        if matches!(verb, "approve" | "reject" | "defer") {
            command.arg("placeholder-org/alpha#11");
        }
        if verb == "reject" {
            command.env("MANDATE_EVENT_TIME", "2026-08-17T00:00:00Z");
        }
        let output = command.output().expect("run queue verb");
        assert_success(&output);
        assert_eq!(output.stdout, fixture(expected));
        assert!(output.stderr.is_empty());
        if verb == "reject" {
            assert_eq!(
                fs::read(home.path().join("selector-events.jsonl")).unwrap(),
                fixture("queue.reject-event.expected.jsonl")
            );
        }
    }

    let home = queue_home();
    let output = ostrom(home.path())
        .args(["queue", "approve", "placeholder-org/alpha#999"])
        .output()
        .expect("run unknown queue mutation");
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, fixture("queue.unknown.expected.stderr.txt"));
    assert_eq!(
        fs::read(home.path().join("queue.jsonl")).unwrap(),
        fixture("queue.jsonl")
    );
}

#[test]
fn trace_verbs_match_shell_records_and_keep_narration_separate() {
    let home = tempdir().expect("trace home");
    let append = ostrom(home.path())
        .env("MANDATE_TRACE_TIME", "2026-08-17T01:02:03Z")
        .args([
            "trace",
            "append",
            "placeholder-event",
            r#"{"repository":"placeholder-org/alpha","count":2}"#,
            r#"{"summary":"placeholder narration"}"#,
        ])
        .output()
        .expect("append trace");
    assert_success(&append);
    let expected = fixture("trace.append.expected.jsonl");
    assert_eq!(append.stdout, expected);
    assert_eq!(
        fs::read(home.path().join("sprint.jsonl")).unwrap(),
        expected
    );

    let facts = ostrom(home.path())
        .args(["trace", "read"])
        .output()
        .expect("read facts");
    assert_success(&facts);
    assert_eq!(facts.stdout, fixture("trace.read.expected.jsonl"));
    assert!(!String::from_utf8_lossy(&facts.stdout).contains("narration"));

    let narration = ostrom(home.path())
        .args(["trace", "read-narration"])
        .output()
        .expect("read narration");
    assert_success(&narration);
    assert_eq!(
        narration.stdout,
        fixture("trace.read-narration.expected.jsonl")
    );
    assert!(!String::from_utf8_lossy(&narration.stdout).contains("\"fact\""));
}

#[test]
fn rust_reader_consumes_shell_written_trace_and_malformed_append_is_atomic() {
    let home = tempdir().expect("trace home");
    let trace = home.path().join("sprint.jsonl");
    fs::write(&trace, fixture("trace.append.expected.jsonl")).unwrap();
    let read = ostrom(home.path())
        .args(["trace", "read"])
        .output()
        .expect("read shell trace");
    assert_success(&read);
    assert_eq!(read.stdout, fixture("trace.read.expected.jsonl"));

    let before = fs::read(&trace).unwrap();
    let malformed = ostrom(home.path())
        .args(["trace", "append", "malformed", "{broken", "{}"])
        .output()
        .expect("reject malformed append");
    assert_eq!(malformed.status.code(), Some(2));
    assert!(malformed.stdout.is_empty());
    assert_eq!(
        malformed.stderr,
        fixture("trace.malformed.expected.stderr.txt")
    );
    assert_eq!(
        fs::read(trace).unwrap(),
        before,
        "no partial row was written"
    );
}

#[test]
fn exactly_one_real_process_acquires_a_named_lease() {
    let home = tempdir().expect("lease home");
    let mut children = Vec::new();
    for index in 0..16 {
        children.push(
            ostrom(home.path())
                .env("MANDATE_LEASE_NAME", "builder.lease")
                .env("MANDATE_LEASE_NOW_EPOCH", "100")
                .args([
                    "lease",
                    "acquire",
                    &format!("placeholder-owner-{index}"),
                    "60",
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn competing process"),
        );
    }
    let statuses = children
        .into_iter()
        .map(|mut child| child.wait().expect("wait for competitor"))
        .collect::<Vec<_>>();
    assert_eq!(
        statuses.iter().filter(|status| status.success()).count(),
        1,
        "exactly one O_EXCL create must win: {statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .filter(|status| !status.success())
            .all(|status| status.code() == Some(3))
    );
}

#[test]
fn lease_verbs_match_shell_and_release_is_role_isolated() {
    let home = tempdir().expect("lease home");
    let acquire = ostrom(home.path())
        .env("MANDATE_LEASE_NAME", "builder.lease")
        .env("MANDATE_LEASE_NOW_EPOCH", "100")
        .args(["lease", "acquire", "placeholder-owner", "60"])
        .output()
        .expect("acquire lease");
    assert_success(&acquire);
    assert_eq!(acquire.stdout, fixture("lease.acquire.expected.jsonl"));
    let status = ostrom(home.path())
        .env("MANDATE_LEASE_NAME", "builder.lease")
        .args(["lease", "status"])
        .output()
        .expect("lease status");
    assert_success(&status);
    assert_eq!(status.stdout, fixture("lease.status.expected.jsonl"));

    let held = ostrom(home.path())
        .env("MANDATE_LEASE_NAME", "builder.lease")
        .env("MANDATE_LEASE_NOW_EPOCH", "120")
        .args(["lease", "acquire", "other-placeholder-owner", "60"])
        .output()
        .expect("held lease refuses another owner");
    assert_eq!(held.status.code(), Some(3));
    assert_eq!(held.stderr, fixture("lease.held.expected.stderr.txt"));

    let mismatch = ostrom(home.path())
        .env("MANDATE_LEASE_NAME", "builder.lease")
        .args(["lease", "release", "other-placeholder-owner"])
        .output()
        .expect("owner mismatch");
    assert_eq!(mismatch.status.code(), Some(3));
    assert_eq!(
        mismatch.stderr,
        fixture("lease.owner-mismatch.expected.stderr.txt")
    );

    let gatekeeper = ostrom(home.path())
        .env("MANDATE_LEASE_NAME", "gatekeeper.lease")
        .env("MANDATE_LEASE_NOW_EPOCH", "100")
        .args(["lease", "acquire", "placeholder-gatekeeper", "60"])
        .output()
        .expect("acquire other role");
    assert_success(&gatekeeper);
    let other_lease = home.path().join("gatekeeper.lease");
    let other_guard = home.path().join(".gatekeeper.lease.guard");
    fs::write(&other_guard, "placeholder-guard\n").unwrap();
    let other_bytes = fs::read(&other_lease).unwrap();

    let release = ostrom(home.path())
        .env("MANDATE_LEASE_NAME", "builder.lease")
        .args(["lease", "release", "placeholder-owner"])
        .output()
        .expect("release builder lease");
    assert_success(&release);
    assert!(release.stdout.is_empty() && release.stderr.is_empty());
    assert_eq!(fs::read(other_lease).unwrap(), other_bytes);
    assert_eq!(fs::read(other_guard).unwrap(), b"placeholder-guard\n");
}

#[test]
fn work_order_verbs_match_shell_fixtures_and_validate_bash_era_orders() {
    let home = tempdir().expect("work order home");
    for (verb, expected) in [
        ("item-hash", "work-order.item-hash.expected.txt"),
        ("branch-name", "work-order.branch-name.expected.txt"),
    ] {
        let output = ostrom(home.path())
            .args(["work-order", verb, "placeholder-org/alpha#42"])
            .output()
            .expect("derive work order identifier");
        assert_success(&output);
        assert_eq!(output.stdout, fixture(expected));
    }
    let bash_era = fixture_path("work-order.bash-era.json");
    let validate = ostrom(home.path())
        .args(["work-order", "validate", bash_era.to_str().unwrap()])
        .output()
        .expect("validate Bash-era order");
    assert_success(&validate);
    assert!(validate.stdout.is_empty() && validate.stderr.is_empty());

    let create = ostrom(home.path())
        .env("MANDATE_TRACE_TIME", "2026-08-17T01:02:03Z")
        .env("MANDATE_ORDER_COST_CEILING_USD", "12.5")
        .env("MANDATE_ORDER_TOKEN_CEILING", "12345")
        .args([
            "work-order",
            "create",
            fixture_path("work-order-candidate.json").to_str().unwrap(),
        ])
        .output()
        .expect("create work order");
    assert_success(&create);
    assert_eq!(
        create.stderr,
        fixture("work-order.create.expected.stderr.txt")
    );
    let target = PathBuf::from(String::from_utf8(create.stdout).unwrap().trim());
    assert_eq!(
        target.parent(),
        Some(home.path().join("work-orders").as_path())
    );
    assert!(
        target.ends_with(
            String::from_utf8(fixture("work-order.create.expected.path-suffix.txt"))
                .unwrap()
                .trim()
        ),
        "create stdout retained the shell-recorded target suffix: {}",
        target.display()
    );
    let validate_created = ostrom(home.path())
        .args(["work-order", "validate", target.to_str().unwrap()])
        .output()
        .expect("validate created order");
    assert_success(&validate_created);

    let malformed = home.path().join("malformed.json");
    fs::write(&malformed, "{\"schema_version\":1}\n").unwrap();
    let invalid = ostrom(home.path())
        .args(["work-order", "validate", malformed.to_str().unwrap()])
        .output()
        .expect("reject malformed order");
    assert_eq!(invalid.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(invalid.stderr).unwrap(),
        format!(
            "ostrom work order: invalid schema_version 1 work order at {}\n",
            malformed.display()
        )
    );
}

fn queue_home() -> TempDir {
    let home = tempdir().expect("queue home");
    fs::write(home.path().join("queue.jsonl"), fixture("queue.jsonl")).unwrap();
    fs::write(home.path().join("state.json"), fixture("state.json")).unwrap();
    home
}

fn ostrom(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
    command.env("OSTROM_HOME", home);
    command
}

fn fixture(name: &str) -> Vec<u8> {
    fs::read(fixture_path(name)).expect("read recorded shell fixture")
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(FIXTURES).join(name)
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
