#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    state: PathBuf,
    claude: PathBuf,
}

impl Fixture {
    fn new(script: &str) -> Self {
        let root = tempfile::tempdir().expect("temporary pass fixture");
        let state = root.path().join("ostrom");
        fs::create_dir_all(state.join("roles")).expect("create role settings");
        fs::write(state.join("roles/builder.settings.json"), "{}\n").expect("write settings");
        fs::write(state.join("loop-armed"), "").expect("arm pass");
        fs::write(state.join("builder-pass-id"), "a1b2c3d4\n").expect("write id");
        fs::write(state.join("builder-wake-counter"), "6\n").expect("write wake");
        let claude = root.path().join("claude-stub");
        fs::write(&claude, format!("#!/usr/bin/env bash\n{script}\n")).expect("write stub");
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).expect("chmod stub");
        Self {
            root,
            state,
            claude,
        }
    }

    fn command(&self) -> Command {
        self.command_for("builder")
    }

    fn command_for(&self, role: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        command
            .args(["pass", role])
            .env_clear()
            .env("OSTROM_HOME", &self.state)
            .env("CLAUDE_CONFIG_DIR", self.root.path())
            .env("HOME", self.root.path())
            .env("PATH", env::var_os("PATH").unwrap_or_default())
            .env("CLAUDE_BIN", &self.claude)
            .env("MANDATE_TRACE_TIME", "2026-08-01T00:00:00Z")
            .env("MANDATE_SWEEP_TIME", "2026-08-01T00:00:00Z")
            .env("MANDATE_TODAY", "2026-08-01")
            .env("MANDATE_NOW_EPOCH", "1785542400")
            .env("MANDATE_LEASE_NOW_EPOCH", "1785542400");
        command
    }

    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(self.state.join("sprint.jsonl"))
            .expect("read trace")
            .lines()
            .map(|line| serde_json::from_str(line).expect("trace JSON"))
            .collect()
    }

    fn assert_released(&self) {
        assert!(!self.state.join("builder-pass.lease").exists());
        let trace = self.trace();
        assert_eq!(
            trace.last().and_then(|row| row["kind"].as_str()),
            Some("pass-ended")
        );
    }
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if path.exists() && fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {}", path.display());
}

fn signal(pid: u32, name: &str) {
    assert!(
        Command::new("kill")
            .args([format!("-{name}"), pid.to_string()])
            .status()
            .expect("send signal")
            .success()
    );
}

fn wait(mut child: Child) -> ExitStatus {
    child.wait().expect("wait for pass")
}

/// ostrom#99, the production path: every other test here pins `MANDATE_NOW_EPOCH`
/// so simulated days are deterministic, and production sets it **nowhere**. A
/// suite that pins it universally therefore never executes the branch the loop
/// actually takes.
///
/// This is not a hypothetical. ostrom#323 was exactly this shape one variable
/// over: every dispatch test set `OSTROM_HOME` or `CLAUDE_CONFIG_DIR`, the bug
/// required neither to be set, and it took the loop down for 48 hours while CI
/// stayed green. The bash suite guarded the clock case deliberately, with its
/// own fixture and an explicit `env -u MANDATE_NOW_EPOCH`; that guard must not
/// be lost in the move to Rust.
///
/// The claim is "the real clock, not the simulated day" — not a specific date —
/// so both sides of a UTC midnight crossing are accepted rather than letting a
/// midnight run flake.
#[test]
fn an_unpinned_clock_stamps_pass_rows_with_the_real_date() {
    let fixture = Fixture::new("exit 0");

    let before = chrono_free_utc_date();
    let status = fixture
        .command()
        .env_remove("MANDATE_NOW_EPOCH")
        .env_remove("MANDATE_TRACE_TIME")
        .env_remove("MANDATE_TODAY")
        .status()
        .expect("run pass with an unpinned clock");
    assert!(status.success());
    let after = chrono_free_utc_date();

    let trace = fixture.trace();
    let stamps = trace
        .iter()
        .filter(|row| {
            matches!(
                row["kind"].as_str(),
                Some("pass-started") | Some("pass-ended")
            )
        })
        .map(|row| {
            row["ts"]
                .as_str()
                .expect("row carries a timestamp")
                .get(..10)
                .expect("timestamp starts with a date")
                .to_owned()
        })
        .collect::<Vec<_>>();

    assert!(
        !stamps.is_empty(),
        "the pass wrote no pass-started/pass-ended rows"
    );
    for stamp in &stamps {
        assert!(
            stamp == &before || stamp == &after,
            "row stamped {stamp}, which is neither {before} nor {after} — the pass \
             is reading a simulated clock on the path production takes"
        );
        assert_ne!(
            stamp, "2026-08-01",
            "row carries the suite's pinned fixture day even though the clock was \
             unpinned; the pinned value is leaking into the production path"
        );
    }
}

/// `%Y-%m-%d` for now, without taking a chrono dependency in the test crate.
fn chrono_free_utc_date() -> String {
    let output = Command::new("date")
        .args(["-u", "+%Y-%m-%d"])
        .output()
        .expect("read the real UTC date");
    String::from_utf8(output.stdout)
        .expect("date is UTF-8")
        .trim()
        .to_owned()
}

#[test]
fn recorded_shell_output_is_byte_identical() {
    let fixture = Fixture::new(concat!(
        "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-placeholder-session-wake7\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\"\n",
        "printf '%s\\n' '{\"type\":\"result\",\"total_cost_usd\":1.25}'"
    ));
    let output = fixture.command().output().expect("run pass");
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        include_bytes!("fixtures/pass/builder-stdout.expected.txt")
    );
    assert_eq!(
        output.stderr,
        include_bytes!("fixtures/pass/builder-stderr.expected.txt")
    );
    assert_eq!(
        fs::read(fixture.state.join("sprint.jsonl")).expect("read native trace"),
        include_bytes!("fixtures/pass/builder-trace.expected.jsonl")
    );
    fixture.assert_released();
}

/// The recorded fixture compares the trace byte for byte, so any wall-clock
/// field in it is a latent flake. This is the case that actually failed in CI:
/// the same pass emitted `duration_seconds` 0 on an idle machine and 1 under
/// load. Forcing the pass to take over a second must not change a single byte.
#[test]
fn a_slow_pass_records_the_same_bytes_under_a_pinned_clock() {
    let fixture = Fixture::new(concat!(
        "sleep 2\n",
        "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-placeholder-session-wake7\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\"\n",
        "printf '%s\\n' '{\"type\":\"result\",\"total_cost_usd\":1.25}'"
    ));
    let output = fixture.command().output().expect("run pass");
    assert!(output.status.success());
    assert_eq!(
        fs::read(fixture.state.join("sprint.jsonl")).expect("read native trace"),
        include_bytes!("fixtures/pass/builder-trace.expected.jsonl")
    );
    fixture.assert_released();
}

#[test]
fn error_exit_releases_and_finalizes() {
    let fixture = Fixture::new("exit 42");
    let status = fixture.command().status().expect("run failing pass");
    assert_eq!(status.code(), Some(42));
    fixture.assert_released();
    assert_eq!(fixture.trace().last().unwrap()["fact"]["outcome"], "failed");
}

#[test]
fn panic_releases_and_finalizes() {
    let fixture = Fixture::new("exit 0");
    let status = fixture
        .command()
        .env("OSTROM_PASS_TEST_PANIC", "1")
        .status()
        .expect("run panicking pass");
    assert!(!status.success());
    fixture.assert_released();
    assert_eq!(fixture.trace().last().unwrap()["fact"]["outcome"], "failed");
}

#[test]
fn sigterm_releases_finalizes_and_kills_the_process_group() {
    let fixture = Fixture::new(concat!(
        "printf '%s\\n' \"$$\" >\"$OSTROM_HOME/child.pid\"\n",
        "(trap '' TERM; while :; do sleep 1; done) &\n",
        "printf '%s\\n' \"$!\" >\"$OSTROM_HOME/grandchild.pid\"\n",
        "trap 'exit 143' TERM\n",
        "while :; do sleep 1; done"
    ));
    let child = fixture.command().spawn().expect("start pass");
    wait_for(&fixture.state.join("grandchild.pid"));
    signal(child.id(), "TERM");
    let status = wait(child);
    assert_eq!(status.code(), Some(143));
    fixture.assert_released();
    assert_eq!(
        fixture.trace().last().unwrap()["fact"]["outcome"],
        "timed-out"
    );
    let grandchild =
        fs::read_to_string(fixture.state.join("grandchild.pid")).expect("read grandchild pid");
    assert!(
        !Command::new("kill")
            .args(["-0", grandchild.trim()])
            .status()
            .expect("probe grandchild")
            .success()
    );
}

#[test]
fn killed_child_releases_and_finalizes() {
    let fixture = Fixture::new("kill -KILL $$");
    let status = fixture.command().status().expect("run pass");
    assert!(!status.success());
    fixture.assert_released();
}

#[test]
fn orphaned_worker_does_not_retain_the_lease() {
    let fixture = Fixture::new(concat!(
        "printf '%s\\n' \"$$\" >\"$OSTROM_HOME/child.pid\"\n",
        "trap 'exit 143' TERM\n",
        "while :; do sleep 1; done"
    ));
    let child = fixture.command().spawn().expect("start pass");
    wait_for(&fixture.state.join("child.pid"));
    signal(child.id(), "KILL");
    let _ = wait(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && fixture.state.join("builder-pass.lease").exists() {
        thread::sleep(Duration::from_millis(25));
    }
    fixture.assert_released();
}

#[test]
fn sigterm_cleanup_does_not_depend_on_sh() {
    let fixture = Fixture::new(concat!(
        "printf '%s\\n' \"$$\" >\"$OSTROM_HOME/child.pid\"\n",
        "trap 'exit 143' TERM\n",
        "while :; do sleep 1; done"
    ));
    let path = fixture.root.path().join("path-without-sh");
    fs::create_dir(&path).expect("create isolated PATH");
    symlink("/bin/bash", path.join("bash")).expect("link bash");
    symlink("/bin/sleep", path.join("sleep")).expect("link sleep");
    let child = fixture
        .command()
        .env("PATH", &path)
        .spawn()
        .expect("start pass without sh on PATH");
    wait_for(&fixture.state.join("child.pid"));
    signal(child.id(), "TERM");
    assert_eq!(wait(child).code(), Some(143));
    fixture.assert_released();

    let binary = fs::read(env!("CARGO_BIN_EXE_ostrom")).expect("read built binary");
    assert!(!binary.windows(5).any(|window| window == b"sh -c"));
    assert!(
        !binary
            .windows(b"signal_file=$1".len())
            .any(|window| window == b"signal_file=$1")
    );
}

#[test]
fn disarmed_and_outer_lease_held_passes_do_not_spawn_or_trace() {
    let disarmed = Fixture::new("touch \"$OSTROM_TEST_MARKER\"");
    let marker = disarmed.root.path().join("spawned");
    fs::remove_file(disarmed.state.join("loop-armed")).expect("disarm fixture");
    let output = disarmed
        .command()
        .env("OSTROM_TEST_MARKER", &marker)
        .output()
        .expect("run disarmed pass");
    assert!(output.status.success());
    assert!(!marker.exists());
    assert!(!disarmed.state.join("sprint.jsonl").exists());

    let held = Fixture::new("touch \"$OSTROM_TEST_MARKER\"");
    let marker = held.root.path().join("spawned");
    fs::write(
        held.state.join("builder-pass.lease"),
        "{\"owner\":\"fixture-holder\",\"started_at\":1785542300,\"expires_at\":1785546000}\n",
    )
    .expect("write held outer lease");
    let output = held
        .command()
        .env("OSTROM_TEST_MARKER", &marker)
        .output()
        .expect("run overlapping pass");
    assert!(output.status.success());
    assert!(!marker.exists());
    assert!(!held.state.join("sprint.jsonl").exists());
}

#[test]
fn roles_receive_their_permission_modes_and_wakes_retain_one_identity() {
    let fixture = Fixture::new(concat!(
        "printf '%s\\n' \"$@\" >\"$OSTROM_TEST_ARGS\"\n",
        "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-inner-wake\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\"\n",
        "printf '%s\\n' '{\"type\":\"result\",\"total_cost_usd\":1.25}'"
    ));
    let arguments = fixture.root.path().join("arguments");
    for _ in 0..2 {
        let status = fixture
            .command()
            .env("OSTROM_TEST_ARGS", &arguments)
            .status()
            .expect("run builder pass");
        assert!(status.success());
    }
    let args = fs::read_to_string(&arguments).expect("read builder arguments");
    assert!(args.contains("--permission-mode\nauto\n"));
    assert!(args.contains("--max-turns\n200\n"));
    assert!(!args.lines().any(|line| line == "default" || line == "40"));
    let trace = fixture.trace();
    let wrapper_owners = trace
        .iter()
        .filter(|row| row["kind"] == "pass-started")
        .filter_map(|row| row["fact"]["owner"].as_str())
        .filter(|owner| *owner != "builder-inner-wake")
        .collect::<Vec<_>>();
    assert_eq!(wrapper_owners.len(), 2);
    assert!(wrapper_owners[0].ends_with("-wake7"));
    assert!(wrapper_owners[1].ends_with("-wake8"));
    assert_eq!(
        wrapper_owners[0].rsplit_once("-wake").unwrap().0,
        wrapper_owners[1].rsplit_once("-wake").unwrap().0
    );

    let gatekeeper = Fixture::new("printf '%s\\n' \"$@\" >\"$OSTROM_TEST_ARGS\"");
    fs::write(
        gatekeeper.state.join("roles/gatekeeper.settings.json"),
        "{}\n",
    )
    .expect("write gatekeeper settings");
    let gatekeeper_arguments = gatekeeper.root.path().join("gatekeeper-arguments");
    assert!(
        gatekeeper
            .command_for("gatekeeper")
            .env("OSTROM_TEST_ARGS", &gatekeeper_arguments)
            .status()
            .expect("run gatekeeper pass")
            .success()
    );
    let args = fs::read_to_string(gatekeeper_arguments).expect("read gatekeeper arguments");
    assert!(args.contains("--permission-mode\nmanual\n"));
    assert!(!args.lines().any(|line| line == "default"));
}

#[test]
fn wrapper_outcome_follows_inner_protocol_evidence() {
    let cases = [
        ("", true, "no-op", Some("blocked")),
        (
            "printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"permission_denials\":[{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"ostrom sweep\"}}]}'",
            true,
            "permission-denied",
            None,
        ),
        ("exit 42", false, "failed", None),
        (
            concat!(
                "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-inner-wake1\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\"\n",
                "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-ended\",\"fact\":{\"outcome\":\"failed\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\""
            ),
            true,
            "failed",
            None,
        ),
        (
            concat!(
                "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-inner-wake1\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\"\n",
                "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-ended\",\"fact\":{\"outcome\":\"completed\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\""
            ),
            true,
            "completed",
            None,
        ),
    ];
    for (script, success, outcome, reason) in cases {
        let fixture = Fixture::new(script);
        let output = fixture.command().output().expect("run outcome case");
        assert_eq!(output.status.success(), success, "{outcome}");
        let terminal = fixture.trace().pop().expect("terminal trace row");
        assert_eq!(terminal["fact"]["outcome"], outcome);
        assert_eq!(terminal["fact"]["reason"].as_str(), reason);
    }

    let gatekeeper = Fixture::new("");
    fs::write(
        gatekeeper.state.join("roles/gatekeeper.settings.json"),
        "{}\n",
    )
    .expect("write gatekeeper settings");
    assert!(
        gatekeeper
            .command_for("gatekeeper")
            .status()
            .expect("run gatekeeper no-op")
            .success()
    );
    let terminal = gatekeeper.trace().pop().expect("gatekeeper terminal");
    assert_eq!(terminal["fact"]["outcome"], "no-op");
    assert_eq!(terminal["fact"]["reason"], "blocked");
}

#[test]
fn permission_denial_overrides_partial_inner_protocol_evidence() {
    let fixture = Fixture::new(concat!(
        "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-inner-wake1\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\"\n",
        "printf '%s\\n' '{\"type\":\"result\",\"total_cost_usd\":0.5,\"permission_denials\":[{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"ostrom repair-prs owner\"}}]}'"
    ));
    assert!(
        fixture
            .command()
            .status()
            .expect("run denied pass")
            .success()
    );
    let terminal = fixture.trace().pop().expect("denied terminal row");
    assert_eq!(terminal["fact"]["outcome"], "permission-denied");
    assert!(terminal["fact"].get("reason").is_none());
    assert_eq!(terminal["fact"]["cost_usd"], 0.5);
}

#[test]
fn inner_lease_cleanup_distinguishes_child_and_preexisting_owners() {
    let acquired = Fixture::new(concat!(
        "printf '{\"owner\":\"builder-child\",\"started_at\":%s,\"expires_at\":9999999999}\\n' \"$MANDATE_NOW_EPOCH\" >\"$OSTROM_HOME/builder.lease\"\n",
        "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-inner-wake1\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\""
    ));
    assert!(
        acquired
            .command()
            .status()
            .expect("run acquired lease")
            .success()
    );
    assert!(!acquired.state.join("builder.lease").exists());

    let preexisting = Fixture::new("");
    fs::write(
        preexisting.state.join("builder.lease"),
        "{\"owner\":\"interactive-builder\",\"started_at\":1,\"expires_at\":9999999999}\n",
    )
    .expect("write preexisting inner lease");
    assert!(
        preexisting
            .command()
            .status()
            .expect("run preexisting lease")
            .success()
    );
    assert!(preexisting.state.join("builder.lease").exists());
    let terminal = preexisting.trace().pop().expect("preexisting terminal");
    assert_eq!(terminal["fact"]["outcome"], "no-op");
    assert_eq!(terminal["fact"]["reason"], "lease-held");
}

#[test]
fn daily_cap_uses_only_valid_costs_on_the_pinned_day() {
    let trace = concat!(
        "{\"ts\":\"2026-07-31T23:59:59Z\",\"kind\":\"pass-ended\",\"fact\":{\"cost_usd\":999},\"narration\":{}}\n",
        "{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-ended\",\"fact\":{\"cost_usd\":7},\"narration\":{}}\n",
        "{\"ts\":\"2026-08-01T00:00:01Z\",\"kind\":\"pass-ended\",\"fact\":{\"cost_usd\":\"bad\"},\"narration\":{}}\n",
        "{\"ts\":\"2026-08-01T00:00:02Z\",\"kind\":\"pass-ended\",\"fact\":{},\"narration\":{}}\n"
    );
    for (cap, spawned, outcome, reason) in [
        ("8", true, "completed", None),
        ("7", false, "no-op", Some("daily-cap")),
        ("not-a-number", true, "completed", None),
    ] {
        let fixture = Fixture::new(concat!(
            "touch \"$OSTROM_TEST_MARKER\"\n",
            "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-inner-wake1\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\""
        ));
        fs::write(fixture.state.join("sprint.jsonl"), trace).expect("write spend trace");
        let marker = fixture.root.path().join("spawned");
        assert!(
            fixture
                .command()
                .env("MANDATE_DAILY_CAP_USD", cap)
                .env("OSTROM_TEST_MARKER", &marker)
                .status()
                .expect("run spend case")
                .success()
        );
        assert_eq!(marker.exists(), spawned, "cap {cap}");
        let terminal = fixture.trace().pop().expect("spend terminal");
        assert_eq!(terminal["fact"]["outcome"], outcome, "cap {cap}");
        assert_eq!(terminal["fact"]["reason"].as_str(), reason, "cap {cap}");
        assert_eq!(terminal["ts"], "2026-08-01T00:00:00Z");
        assert_eq!(terminal["fact"]["duration_seconds"], 0);
    }
}

const HOSTILE_ENV_CHILD: &str = "OSTROM_TEST_HOSTILE_ENV_CHILD";

fn normalize_libtest_durations(output: &[u8]) -> Vec<u8> {
    const SUMMARY_PREFIX: &[u8] = b"test result: ";
    const DURATION_PREFIX: &[u8] = b"; finished in ";

    let mut normalized = Vec::with_capacity(output.len());
    for line in output.split_inclusive(|byte| *byte == b'\n') {
        if !line.starts_with(SUMMARY_PREFIX) {
            normalized.extend_from_slice(line);
            continue;
        }

        let Some(prefix_offset) = line
            .windows(DURATION_PREFIX.len())
            .position(|window| window == DURATION_PREFIX)
        else {
            normalized.extend_from_slice(line);
            continue;
        };
        let duration_start = prefix_offset + DURATION_PREFIX.len();
        let Some(seconds_offset) = line[duration_start..].iter().position(|byte| *byte == b's')
        else {
            normalized.extend_from_slice(line);
            continue;
        };
        let duration_end = duration_start + seconds_offset;
        let duration = &line[duration_start..duration_end];
        if duration.is_empty()
            || !duration
                .iter()
                .all(|byte| byte.is_ascii_digit() || *byte == b'.')
        {
            normalized.extend_from_slice(line);
            continue;
        }

        normalized.extend_from_slice(&line[..duration_start]);
        normalized.extend_from_slice(b"<duration>");
        normalized.extend_from_slice(&line[duration_end..]);
    }
    normalized
}

#[test]
fn libtest_duration_normalization_is_narrow() {
    let output = b"test result: ok. 1 passed; 0 failed; finished in 0.14s\n\
operator result: finished in 0.14s\n";
    assert_eq!(
        normalize_libtest_durations(output),
        b"test result: ok. 1 passed; 0 failed; finished in <duration>s\n\
operator result: finished in 0.14s\n"
    );

    let changed_result = b"test result: FAILED. 0 passed; 1 failed; finished in 0.20s\n\
operator result: finished in 0.14s\n";
    assert_ne!(
        normalize_libtest_durations(output),
        normalize_libtest_durations(changed_result)
    );
}

#[test]
fn polluted_operator_environment_cannot_change_a_pass_result() {
    if env::var_os(HOSTILE_ENV_CHILD).is_some() {
        let fixture = Fixture::new("");
        let output = fixture.command().output().expect("run hermetic pass");
        assert!(output.status.success());
        let terminal = fixture.trace().pop().expect("hermetic terminal");
        assert_eq!(terminal["fact"]["outcome"], "no-op");
        assert_eq!(terminal["fact"]["reason"], "blocked");
        return;
    }

    let executable = env::current_exe().expect("current integration test executable");
    let run = |polluted: bool| {
        let mut command = Command::new(&executable);
        command
            .env_clear()
            .env("PATH", env::var_os("PATH").unwrap_or_default())
            .env(HOSTILE_ENV_CHILD, "1")
            .args([
                "--exact",
                "polluted_operator_environment_cannot_change_a_pass_result",
                "--nocapture",
            ]);
        if polluted {
            for name in [
                "OSTROM_HOME",
                "CLAUDE_CONFIG_DIR",
                "ANTHROPIC_API_KEY",
                "MANDATE_SEMANTIC_DERIVER",
                "MANDATE_SEMANTIC_MODEL",
                "MANDATE_DAILY_CAP_USD",
                "MANDATE_LEASE_NAME",
                "MANDATE_TRACE_TIME",
                "MANDATE_NOW_EPOCH",
                "MANDATE_MAX_IMPLEMENTERS",
                "MANDATE_IMPLEMENTER_SOURCE_REPO",
                "MANDATE_GH_AS_BIN",
                "MANDATE_SYSTEMD_RUN_BIN",
            ] {
                command.env(name, "hostile-operator-value");
            }
        }
        command.output().expect("run hermetic child")
    };
    let clean = run(false);
    let polluted = run(true);
    assert!(clean.status.success());
    assert!(polluted.status.success());
    assert_eq!(
        normalize_libtest_durations(&clean.stdout),
        normalize_libtest_durations(&polluted.stdout)
    );
    assert_eq!(
        normalize_libtest_durations(&clean.stderr),
        normalize_libtest_durations(&polluted.stderr)
    );
}
