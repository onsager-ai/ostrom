#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tempfile::TempDir;

mod support;

struct Fixture {
    root: TempDir,
    state: PathBuf,
    claude: PathBuf,
    manifest: PathBuf,
    trusted_keys: PathBuf,
}

impl Fixture {
    fn new(script: &str) -> Self {
        Self::with_options(script, false)
    }

    fn with_options(script: &str, run_signature: bool) -> Self {
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
        let manifest = root.path().join("ostrom.yaml");
        let trusted_keys = compose_current(&manifest, &state, run_signature);
        Self {
            root,
            state,
            claude,
            manifest,
            trusted_keys,
        }
    }

    /// Recompose `current` with the dispatchability run signature enabled, for
    /// the no-op tests that also populate the queue/state it hashes.
    fn enable_run_signature(&self) {
        compose_current(&self.manifest, &self.state, true);
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
            .env("OSTROM_POLICY_TRUSTED_KEYS", &self.trusted_keys);
        command
    }

    fn write_blocked_dispatchability_state(&self) {
        fs::write(
            self.state.join("mandates.yaml"),
            r#"provider: file
cadence_hours: 1
stuck_after_days: 7
bounce_all: []
projects:
  - repo: placeholder-org/alpha
    delegated: []
    excluded: []
    reserved: []
    default: delegated
    paused: false
    bounce: []
"#,
        )
        .expect("write mandate fixture");
        fs::write(
            self.state.join("queue.jsonl"),
            concat!(
                "{\"id\":\"placeholder-org/alpha#1\",\"repo\":\"placeholder-org/alpha\",",
                "\"ref\":\"#1\",\"title\":\"Placeholder decision\",\"kind\":\"decision\",",
                "\"mandate\":{\"reason\":\"placeholder\"},\"state\":\"pending\",",
                "\"opened\":\"2026-01-01T00:00:00Z\",\"needs_judgment\":true,\"blocked_by\":[]}\n"
            ),
        )
        .expect("write blocked queue");
        let state = json!({
            "version": 2,
            "work_ranking": [],
            "work_ranking_faults": [],
            "repos": {
                "placeholder-org/alpha": {"ci_drift": {}}
            },
            "dependency_graph": {
                "graph_version": 1,
                "configured_repositories": ["placeholder-org/alpha"],
                "nodes": [{
                    "id": "placeholder-org/alpha#1",
                    "open": true,
                    "dependencies": [],
                    "unsatisfied": [],
                    "children": [],
                    "dispatchable": true,
                    "unblocking_power": 0
                }],
                "edges": [],
                "faults": []
            }
        });
        fs::write(
            self.state.join("state.json"),
            serde_json::to_vec(&state).expect("serialize sweep state"),
        )
        .expect("write sweep state");
    }

    fn approve_blocked_decision(&self) {
        let queue = fs::read_to_string(self.state.join("queue.jsonl"))
            .expect("read blocked queue")
            .replace("\"state\":\"pending\"", "\"state\":\"approved\"");
        fs::write(self.state.join("queue.jsonl"), queue).expect("approve decision");
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

fn compose_current(manifest: &Path, state: &Path, run_signature: bool) -> PathBuf {
    let signature_line = if run_signature {
        "\n    run_signature: dispatchability"
    } else {
        ""
    };
    let manifest_yaml = format!(
        r#"manifest_version: 1
actors: {{builder: {{permission_mode: auto}}}}
operations:
  work:
    steps:
      - uses: agent/claude
        with: {{prompt: "run the pass"}}
grants:
  work: {{actors: builder, operations: work, repositories: placeholder-org/repository}}
loops:
  builder-pass:
    actor: builder
    operation: work
    target: placeholder-org/repository
    every: hourly{signature_line}
"#
    );
    // A repository manifest and a distinct signed operator copy: compose merges
    // the operator's operations/loops in. One file used as both is treated as
    // repository-only and its operations are cleared.
    let repo = manifest.with_file_name("policy.yaml");
    fs::write(&repo, &manifest_yaml).expect("write repository policy");
    let trusted_keys = support::sign_manifest(&repo);
    fs::write(manifest, &manifest_yaml).expect("write operator policy");
    support::sign_manifest(manifest);
    let composed = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .arg("compose")
        .arg(&repo)
        .env("OSTROM_HOME", state)
        .env("OSTROM_POLICY_MANIFEST", manifest)
        .env("OSTROM_POLICY_TRUSTED_KEYS", &trusted_keys)
        .output()
        .expect("compose pass policy");
    assert!(
        composed.status.success(),
        "compose failed: {}",
        String::from_utf8_lossy(&composed.stderr)
    );
    trusted_keys
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

/// ostrom#99, the production path: this test must exercise the realtime clock
/// that the binary constructs, not a deterministic library clock.
///
/// This is not a hypothetical. ostrom#323 was exactly this shape one variable
/// over: every dispatch test set `OSTROM_HOME` or `CLAUDE_CONFIG_DIR`, the bug
/// required neither to be set, and it took the loop down for 48 hours while CI
/// stayed green. The bash suite guarded the clock case deliberately, with its
/// own fixture and an explicit unset clock. That guard must not be lost in the
/// move to Rust. Remove every retired clock name here, including the
/// helper-mediated audit, replay, and excuse clocks, so reintroducing an
/// ambient fixture clock elsewhere cannot silently weaken this test.
///
/// The claim is "the real clock, not the simulated day" — not a specific date —
/// so both sides of a UTC midnight crossing are accepted rather than letting a
/// midnight run flake.
#[test]
fn an_unpinned_clock_stamps_pass_rows_with_the_real_date() {
    let fixture = Fixture::new("exit 0");

    let before = chrono_free_utc_date();
    let mut command = fixture.command();
    for name in [
        "MANDATE_NOW_EPOCH",
        "MANDATE_TRACE_TIME",
        "MANDATE_TODAY",
        "MANDATE_GATE_TIME",
        "MANDATE_SWEEP_TIME",
        "MANDATE_EVENT_TIME",
        "MANDATE_DIGEST_TIME",
        "MANDATE_LEASE_NOW_EPOCH",
        "MANDATE_AUDIT_TIME",
        "MANDATE_REPLAY_TIME",
        "MANDATE_EXCUSE_TIME",
    ] {
        command.env_remove(name);
    }
    let status = command.status().expect("run pass with an unpinned clock");
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
fn recorded_shell_output_matches_apart_from_the_injected_clock() {
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
        normalize_pass_trace(
            &fs::read(fixture.state.join("sprint.jsonl")).expect("read native trace")
        ),
        include_bytes!("fixtures/pass/builder-trace.expected.jsonl")
    );
    fixture.assert_released();
}

/// The recorded fixture compares the trace byte for byte, so any wall-clock
/// field in it is a latent flake. This is the case that actually failed in CI:
/// the same pass emitted `duration_seconds` 0 on an idle machine and 1 under
/// load. Forcing the pass to take over a second must not change a single byte.
#[test]
fn a_slow_pass_changes_only_realtime_duration_and_timestamps() {
    let fixture = Fixture::new(concat!(
        "sleep 2\n",
        "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-placeholder-session-wake7\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\"\n",
        "printf '%s\\n' '{\"type\":\"result\",\"total_cost_usd\":1.25}'"
    ));
    let output = fixture.command().output().expect("run pass");
    assert!(output.status.success());
    assert_eq!(
        normalize_pass_trace(
            &fs::read(fixture.state.join("sprint.jsonl")).expect("read native trace")
        ),
        include_bytes!("fixtures/pass/builder-trace.expected.jsonl")
    );
    fixture.assert_released();
}

fn normalize_pass_trace(bytes: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let mut row: Value = serde_json::from_slice(line).expect("trace JSON");
        row["ts"] = Value::String("2026-08-01T00:00:00Z".to_owned());
        if row["kind"] == "pass-ended" {
            row["fact"]["duration_seconds"] = Value::from(0);
        }
        serde_json::to_writer(&mut normalized, &row).expect("serialize normalized trace");
        normalized.push(b'\n');
    }
    normalized
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
        format!(
            "{{\"owner\":\"fixture-holder\",\"started_at\":1,\"expires_at\":{}}}\n",
            u64::MAX
        ),
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
fn an_unchanged_fully_blocked_backlog_ends_before_spawning_and_records_zero_cost() {
    let fixture = Fixture::new(concat!(
        "printf '%s\\n' spawned >>\"$OSTROM_TEST_MARKER\"\n",
        "printf '%s\\n' '{\"type\":\"result\",\"total_cost_usd\":1.25}'"
    ));
    fixture.write_blocked_dispatchability_state();
    let marker = fixture.root.path().join("spawned");

    for _ in 0..2 {
        assert!(
            fixture
                .command()
                .env("OSTROM_TEST_MARKER", &marker)
                .status()
                .expect("run blocked pass")
                .success()
        );
    }

    assert_eq!(
        fs::read_to_string(&marker)
            .expect("read spawn marker")
            .lines()
            .count(),
        1,
        "the unchanged second pass spawned the agent"
    );
    let ended = fixture
        .trace()
        .into_iter()
        .filter(|row| row["kind"] == "pass-ended")
        .collect::<Vec<_>>();
    assert_eq!(ended.len(), 2);
    assert_eq!(ended[0]["fact"]["cost_usd"], 1.25);
    assert_eq!(ended[0]["fact"]["dispatchable_count"], 0);
    assert_eq!(ended[1]["fact"]["outcome"], "no-op");
    assert_eq!(ended[1]["fact"]["reason"], "no-dispatchable-work-unchanged");
    assert_eq!(ended[1]["fact"]["cost_usd"], 0.0);
    assert_eq!(ended[1]["fact"]["queue_count"], 1);
    assert_eq!(ended[1]["fact"]["dispatchable_count"], 0);
    let hash = ended[1]["fact"]["dispatchability_hash"]
        .as_str()
        .expect("terminal trace carries the snapshot hash");
    assert_eq!(hash.len(), 64);
    assert_eq!(
        fs::read_to_string(fixture.state.join("builder-dispatchability-hash"))
            .expect("read durable snapshot hash")
            .trim_end(),
        hash
    );
    fixture.assert_released();
}

#[test]
fn a_dispatchability_input_change_defeats_the_short_circuit_on_the_next_pass() {
    let fixture = Fixture::new("printf '%s\\n' spawned >>\"$OSTROM_TEST_MARKER\"");
    fixture.write_blocked_dispatchability_state();
    let marker = fixture.root.path().join("spawned");

    assert!(
        fixture
            .command()
            .env("OSTROM_TEST_MARKER", &marker)
            .status()
            .expect("establish blocked snapshot")
            .success()
    );
    fixture.approve_blocked_decision();
    assert!(
        fixture
            .command()
            .env("OSTROM_TEST_MARKER", &marker)
            .status()
            .expect("run immediately after approval")
            .success()
    );

    assert_eq!(
        fs::read_to_string(&marker)
            .expect("read spawn marker")
            .lines()
            .count(),
        2,
        "the approved decision did not spawn on the very next pass"
    );
    let terminal = fixture.trace().pop().expect("changed terminal row");
    assert_eq!(terminal["fact"]["dispatchable_count"], 1);
    assert_ne!(terminal["fact"]["reason"], "no-dispatchable-work-unchanged");
    fixture.assert_released();
}

#[test]
fn a_failed_agent_pass_does_not_establish_a_blocked_snapshot() {
    let fixture = Fixture::new("exit 42");
    fixture.write_blocked_dispatchability_state();
    assert_eq!(
        fixture
            .command()
            .status()
            .expect("run failed blocked pass")
            .code(),
        Some(42)
    );
    assert!(!fixture.state.join("builder-dispatchability-hash").exists());

    fs::write(
        &fixture.claude,
        "#!/usr/bin/env bash\nprintf '%s\\n' spawned >\"$OSTROM_TEST_MARKER\"\n",
    )
    .expect("replace failed agent stub");
    let marker = fixture.root.path().join("retried");
    assert!(
        fixture
            .command()
            .env("OSTROM_TEST_MARKER", &marker)
            .status()
            .expect("retry blocked pass")
            .success()
    );
    assert!(marker.exists(), "the failed pass suppressed its retry");
    fixture.assert_released();
}

#[test]
fn default_branch_turning_green_defeats_the_short_circuit_without_a_candidate() {
    let fixture = Fixture::new("printf '%s\\n' spawned >>\"$OSTROM_TEST_MARKER\"");
    fixture.write_blocked_dispatchability_state();
    let state_path = fixture.state.join("state.json");
    let mut state: Value =
        serde_json::from_slice(&fs::read(&state_path).expect("read state")).expect("parse state");
    state["repos"]["placeholder-org/alpha"]["ci_drift"] = json!({
        "17": {"run_id": 41, "red_since": "2026-07-31T00:00:00Z"}
    });
    fs::write(
        &state_path,
        serde_json::to_vec(&state).expect("serialize red state"),
    )
    .expect("write red state");
    let marker = fixture.root.path().join("spawned");
    assert!(
        fixture
            .command()
            .env("OSTROM_TEST_MARKER", &marker)
            .status()
            .expect("establish red snapshot")
            .success()
    );

    state["repos"]["placeholder-org/alpha"]["ci_drift"] = json!({});
    fs::write(
        &state_path,
        serde_json::to_vec(&state).expect("serialize green state"),
    )
    .expect("write green state");
    assert!(
        fixture
            .command()
            .env("OSTROM_TEST_MARKER", &marker)
            .status()
            .expect("run first green pass")
            .success()
    );

    assert_eq!(
        fs::read_to_string(&marker)
            .expect("read spawn marker")
            .lines()
            .count(),
        2,
        "the default-branch transition did not wake the agent immediately"
    );
    let terminal = fixture.trace().pop().expect("green terminal row");
    assert_eq!(terminal["fact"]["dispatchable_count"], 0);
    assert_ne!(terminal["fact"]["reason"], "no-dispatchable-work-unchanged");
    fixture.assert_released();
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
    assert!(args.contains("# Mandate Work\n"));
    assert!(!args.lines().any(|line| line == "/ostrom:work"));
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
    assert!(args.contains("# Mandate Gatekeep\n"));
    assert!(!args.lines().any(|line| line == "/ostrom:gatekeep"));
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
        "printf '{\"owner\":\"builder-child\",\"started_at\":%s,\"expires_at\":9999999999}\\n' \"$(date +%s)\" >\"$OSTROM_HOME/builder.lease\"\n",
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
fn daily_cap_uses_only_valid_costs_on_the_current_day() {
    for (cap, spawned, outcome, reason) in [
        ("8", true, "completed", None),
        ("7", false, "no-op", Some("daily-cap")),
        ("not-a-number", true, "completed", None),
    ] {
        let fixture = Fixture::new(concat!(
            "touch \"$OSTROM_TEST_MARKER\"\n",
            "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-inner-wake1\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\""
        ));
        let today = chrono_free_utc_date();
        let trace = [
            serde_json::json!({
                "ts": "1900-01-01T23:59:59Z",
                "kind": "pass-ended",
                "fact": {"cost_usd": 999},
                "narration": {}
            }),
            serde_json::json!({
                "ts": format!("{today}T00:00:00Z"),
                "kind": "pass-ended",
                "fact": {"cost_usd": 7},
                "narration": {}
            }),
            serde_json::json!({
                "ts": format!("{today}T00:00:01Z"),
                "kind": "pass-ended",
                "fact": {"cost_usd": "bad"},
                "narration": {}
            }),
            serde_json::json!({
                "ts": format!("{today}T00:00:02Z"),
                "kind": "pass-ended",
                "fact": {},
                "narration": {}
            }),
        ]
        .into_iter()
        .map(|row| format!("{row}\n"))
        .collect::<String>();
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
        assert!(
            terminal["ts"]
                .as_str()
                .is_some_and(|timestamp| timestamp.starts_with(&today))
        );
        assert!(
            terminal["fact"]["duration_seconds"]
                .as_u64()
                .is_some_and(|duration| duration <= 5)
        );
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
