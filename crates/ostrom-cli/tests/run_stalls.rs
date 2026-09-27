//! #619: stalled holds are reaped where ostrom already runs (`ostrom up`
//! here; `ostrom dispatch` in `dispatch_lifecycle.rs`), doctor names them, and
//! the loop supervisor never launches over a live worker.
//!
//! Each hold here is the record shape production writes, around a real
//! process this test starts as its own process-group leader, so the reaper's
//! identity checks and signals meet a real `/proc` entry. Every process is
//! killed by its own group on drop, never by pattern.
#![cfg(unix)]

use std::{
    fs,
    os::unix::{fs::PermissionsExt as _, process::CommandExt as _},
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant, SystemTime},
};

use chrono::{DateTime, SecondsFormat, Utc};
use ethogram::EventDraft;
use serde_json::{Value, json};
use tempfile::TempDir;
use umwelt_runtime::{FileSink, Sink as _};

mod support;

const ITEM: &str = "placeholder-org/alpha#7";
const ORDER: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const UNIT: &str = "ostrom-implementer-stalled";
const RUN: &str = "builder-stalled-placeholder-run";
const PASS_OWNER: &str = "builder-a1b2c3d4-wake3";

/// A process this test owns: its own group leader, killed with its group.
struct Sleeper {
    child: Child,
}

impl Sleeper {
    fn start() -> Self {
        let child = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .stdin(Stdio::null())
            .spawn()
            .expect("start a sleeping process");
        Self { child }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// A process that ignores `SIGTERM`, so only `SIGKILL` stops it.
    fn ignoring_term() -> Self {
        let child = Command::new("sh")
            .args(["-c", "trap '' TERM; exec sleep 60"])
            .process_group(0)
            .stdin(Stdio::null())
            .spawn()
            .expect("start a process that ignores SIGTERM");
        // Only once `sleep` has replaced the shell is the trap in force.
        let comm = format!("/proc/{}/comm", child.id());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fs::read_to_string(&comm).is_ok_and(|name| name.trim() == "sleep") {
            assert!(Instant::now() < deadline, "the shell never became sleep");
            thread::sleep(Duration::from_millis(20));
        }
        Self { child }
    }

    /// Kill it, as a reaper's stop would have, and wait until it is gone.
    fn kill(&mut self) {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", self.child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        assert!(
            self.wait_stopped(Duration::from_secs(5)),
            "the process outlived SIGKILL"
        );
    }

    /// `(process group, start time)` as `/proc` records them.
    fn identity(&self) -> (u32, u64) {
        let stat = fs::read_to_string(format!("/proc/{}/stat", self.pid())).expect("read stat");
        let fields = stat
            .rsplit_once(')')
            .expect("stat command field")
            .1
            .split_whitespace()
            .collect::<Vec<_>>();
        (
            fields[2].parse().expect("process group"),
            fields[19].parse().expect("start time"),
        )
    }

    /// Whether the process is still running, reaping it if it has exited.
    fn running(&mut self) -> bool {
        self.child
            .try_wait()
            .expect("poll sleeping process")
            .is_none()
    }

    fn wait_stopped(&mut self, ceiling: Duration) -> bool {
        let deadline = Instant::now() + ceiling;
        while Instant::now() < deadline {
            if !self.running() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        !self.running()
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", self.child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.child.wait();
    }
}

struct Home {
    root: TempDir,
    path: PathBuf,
    trusted_keys: PathBuf,
}

impl Home {
    fn composed(manifest: &str) -> (Self, String) {
        let root = TempDir::new().expect("temporary ostrom home");
        let path = root.path().join("home");
        fs::create_dir_all(&path).expect("create home");
        let manifest_path = path.join("ostrom.yaml");
        fs::write(&manifest_path, manifest).expect("write operator manifest");
        let trusted_keys = support::sign_manifest(&manifest_path);
        let home = Self {
            root,
            path,
            trusted_keys,
        };
        let output = home
            .command()
            .arg("compose")
            .arg(&manifest_path)
            .output()
            .expect("compose the current policy version");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let digest = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .find_map(|word| word.strip_prefix("digest="))
            .map(str::to_owned)
            .expect("compose names its digest");
        (home, digest)
    }

    fn new() -> Self {
        Self::composed("manifest_version: 1\n").0
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        command
            .current_dir(&self.path)
            .env("OSTROM_HOME", &self.path)
            .env("CLAUDE_CONFIG_DIR", self.root.path())
            .env("OSTROM_POLICY_TRUSTED_KEYS", &self.trusted_keys)
            .env_remove("OSTROM_POLICY_MANIFEST")
            .env_remove("OSTROM_RUN_ID")
            .env_remove("MANDATE_DAILY_CAP_USD");
        command
    }

    fn up(&self) -> Output {
        self.command().arg("up").output().expect("run ostrom up")
    }

    fn doctor_work_orders(&self) -> String {
        let output = self
            .command()
            .args(["doctor", "--check", "work-orders"])
            .output()
            .expect("run doctor work-orders");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn append_trace(&self, row: &Value) {
        let path = self.path.join("sprint.jsonl");
        let mut trace = fs::read_to_string(&path).unwrap_or_default();
        trace.push_str(&format!("{row}\n"));
        fs::write(path, trace).expect("append trace row");
    }

    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(self.path.join("sprint.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("trace row"))
            .collect()
    }

    fn terminal_rows(&self, kind: &str, key: &str, value: &str) -> Vec<Value> {
        self.trace()
            .into_iter()
            .filter(|row| row["kind"] == kind && row["fact"][key] == value)
            .collect()
    }

    fn claim(&self) -> PathBuf {
        self.path.join("reaping").join(format!("{RUN}.claim"))
    }

    fn item_lease(&self) -> PathBuf {
        self.path.join(format!(
            "implementer-item-{}.lease",
            ostrom_store::item_hash(ITEM)
        ))
    }

    /// A process-backend implementer hold on `sleeper`, dispatched a minute
    /// ago, exactly as `ostrom dispatch` records one. `start_time_skew` moves
    /// the recorded start time away from the real one.
    fn implementer_hold(&self, sleeper: &Sleeper, idle_seconds: u64, start_time_skew: u64) {
        let (process_group, start_time) = sleeper.identity();
        let now = epoch_now();
        self.append_trace(&json!({
            "ts": timestamp(now - 60),
            "kind": "work-dispatched",
            "fact": {
                "schema_version": 1,
                "item_id": ITEM,
                "order_id": ORDER,
                "unit_name": UNIT,
                "backend": "process",
                "run_id": RUN,
                "runner": "agent/codex",
                "wall_seconds": 14_400,
                "idle_seconds": idle_seconds,
                "cost_ceiling_usd": 20,
                "token_ceiling": 500_000,
                "cost_usd": null,
                "duration_seconds": 0,
            },
            "narration": {},
        }));
        fs::write(
            self.item_lease(),
            json!({
                "owner": UNIT,
                "started_at": now - 60,
                "expires_at": now + 3_600,
                "pid": sleeper.pid(),
                "process_group_id": process_group,
                "process_start_time": start_time + start_time_skew,
            })
            .to_string(),
        )
        .expect("write the implementer lease");
    }

    /// A systemd-backend implementer hold, dispatched a minute ago: its lease
    /// is time-bound and names no process.
    fn unit_hold(&self, idle_seconds: u64) {
        let now = epoch_now();
        self.append_trace(&json!({
            "ts": timestamp(now - 60),
            "kind": "work-dispatched",
            "fact": {
                "schema_version": 1,
                "item_id": ITEM,
                "order_id": ORDER,
                "unit_name": UNIT,
                "backend": "systemd",
                "run_id": RUN,
                "runner": "agent/codex",
                "wall_seconds": 14_400,
                "idle_seconds": idle_seconds,
                "cost_ceiling_usd": 20,
                "token_ceiling": 500_000,
                "cost_usd": null,
                "duration_seconds": 0,
            },
            "narration": {},
        }));
        fs::write(
            self.item_lease(),
            json!({"owner": UNIT, "started_at": now - 60, "expires_at": now + 3_600}).to_string(),
        )
        .expect("write the unit's lease");
    }

    /// A pass hold: `pass-started` with its run id, the run's `run.started`
    /// with `ceilings`, and, when a process is given, the pass lease naming it.
    fn pass_hold(&self, sleeper: Option<&Sleeper>, ceilings: &Value) {
        let now = epoch_now();
        self.append_trace(&json!({
            "ts": timestamp(now - 30),
            "kind": "pass-started",
            "fact": {"owner": PASS_OWNER, "run_id": RUN},
            "narration": {},
        }));
        FileSink::new(self.path.join("runs"))
            .append(
                RUN,
                EventDraft {
                    event_type: "run.started".to_owned(),
                    payload: json!({
                        "kind": "handoff",
                        "actor": "builder",
                        "harness": "claude",
                        "ceilings": ceilings,
                    }),
                    captured_at: None,
                },
            )
            .expect("append run.started");
        if let Some(sleeper) = sleeper {
            let (process_group, start_time) = sleeper.identity();
            fs::write(
                self.path.join("builder-pass.lease"),
                json!({
                    "owner": PASS_OWNER,
                    "started_at": now - 30,
                    "expires_at": now + 120,
                    "pid": sleeper.pid(),
                    "process_group_id": process_group,
                    "process_start_time": start_time,
                })
                .to_string(),
            )
            .expect("write the pass lease");
        }
    }
}

/// The `reaped=` count `ostrom up` prints.
fn reaped(output: &Output) -> u64 {
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find_map(|word| word.strip_prefix("reaped="))
        .and_then(|count| count.parse().ok())
        .expect("ostrom up prints its reaped count")
}

/// The pid and start time of a process that has exited: a reaper that died.
fn dead_process() -> (u32, u64) {
    let mut child = Sleeper {
        child: Command::new("sleep")
            .arg("0.2")
            .process_group(0)
            .spawn()
            .expect("start a short-lived process"),
    };
    let (_, start_time) = child.identity();
    assert!(child.wait_stopped(Duration::from_secs(5)));
    (child.pid(), start_time)
}

fn epoch_now() -> i64 {
    DateTime::<Utc>::from(SystemTime::now()).timestamp()
}

fn timestamp(epoch: i64) -> String {
    DateTime::<Utc>::from_timestamp(epoch, 0)
        .expect("valid timestamp")
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// #619 acceptance: the same stall `ostrom dispatch` reaps is reaped by
/// `ostrom up`. The process is stopped, the lease released, and the terminal
/// row says `stalled` and charges the order's cost ceiling.
#[test]
fn up_reaps_a_live_silent_implementer_past_its_idle_cap() {
    let home = Home::new();
    let mut sleeper = Sleeper::start();
    home.implementer_hold(&sleeper, 1, 0);

    let output = home.up();
    let stopped = sleeper.wait_stopped(Duration::from_secs(15));

    let failed = home.terminal_rows("work-failed", "order_id", ORDER);
    let row = failed.first().cloned().unwrap_or(Value::Null);
    assert_eq!(
        json!({
            "up_succeeded": output.status.success(),
            "up_reports_it": String::from_utf8_lossy(&output.stdout).contains("reaped=1"),
            "process_stopped": stopped,
            "lease_released": !home.item_lease().exists(),
            "terminal_rows": failed.len(),
            "reason": row["fact"]["reason"],
            "run_id": row["fact"]["run_id"],
            "cost_usd": row["fact"]["cost_usd"].as_f64(),
            "cost_basis": row["fact"]["cost_basis"],
            "reaped": row["fact"]["reaped"],
            "stalled_seconds_past_the_cap": row["fact"]["stalled_seconds"]
                .as_u64()
                .is_some_and(|seconds| seconds > 1),
        }),
        json!({
            "up_succeeded": true,
            "up_reports_it": true,
            "process_stopped": true,
            "lease_released": true,
            "terminal_rows": 1,
            "reason": "stalled",
            "run_id": RUN,
            "cost_usd": 20.0,
            "cost_basis": "declared-ceiling",
            "reaped": true,
            "stalled_seconds_past_the_cap": true,
        }),
        "up stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The kill guard: a lease whose pid is alive but whose start time is not the
/// one recorded names a different process. It is neither signalled nor
/// charged, however stale the hold looks.
#[test]
fn a_recorded_pid_that_is_now_another_process_is_never_killed() {
    let home = Home::new();
    let mut sleeper = Sleeper::start();
    home.implementer_hold(&sleeper, 1, 1);

    let output = home.up();
    thread::sleep(Duration::from_millis(500));

    assert_eq!(
        json!({
            "up_succeeded": output.status.success(),
            "process_running": sleeper.running(),
            "stalled_rows": home
                .terminal_rows("work-failed", "order_id", ORDER)
                .iter()
                .filter(|row| row["fact"]["reason"] == "stalled")
                .count(),
        }),
        json!({
            "up_succeeded": true,
            "process_running": true,
            "stalled_rows": 0,
        }),
        "up stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Progress includes the transcript's modification time: a Codex-style run
/// that writes no run events but keeps writing its transcript is working, and
/// is not reaped however long ago it was dispatched.
#[test]
fn a_silent_run_whose_transcript_advances_is_not_reaped() {
    let home = Home::new();
    let mut sleeper = Sleeper::start();
    home.implementer_hold(&sleeper, 5, 0);
    let transcript = home.path.join("implementer-runs").join(ORDER);
    fs::create_dir_all(&transcript).expect("create the transcript directory");
    fs::write(
        transcript.join("events.jsonl"),
        "{\"type\":\"item.started\"}\n",
    )
    .expect("write the transcript now");

    let output = home.up();

    assert_eq!(
        json!({
            "up_succeeded": output.status.success(),
            "process_running": sleeper.running(),
            "terminal_rows": home.terminal_rows("work-failed", "order_id", ORDER).len(),
            "lease_held": home.item_lease().exists(),
        }),
        json!({
            "up_succeeded": true,
            "process_running": true,
            "terminal_rows": 0,
            "lease_held": true,
        }),
        "up stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// doctor's `work-orders` check fails on the stalled hold, naming its run, and
/// passes once `ostrom up` has reaped it: one definition of "stalled".
#[test]
fn doctor_fails_on_a_stalled_hold_and_passes_once_it_is_reaped() {
    let home = Home::new();
    let mut sleeper = Sleeper::start();
    home.implementer_hold(&sleeper, 1, 0);

    let before = home.doctor_work_orders();
    let up = home.up();
    let stopped = sleeper.wait_stopped(Duration::from_secs(15));
    let after = home.doctor_work_orders();

    assert_eq!(
        json!({
            "before_fails": before.starts_with("FAIL|work-orders|stalled hold:"),
            "before_names_the_run": before.contains(&format!("run={RUN}")),
            "up_succeeded": up.status.success(),
            "stopped": stopped,
            "after_passes": after.starts_with("OK|work-orders|"),
        }),
        json!({
            "before_fails": true,
            "before_names_the_run": true,
            "up_succeeded": true,
            "stopped": true,
            "after_passes": true,
        }),
        "before: {before}\nafter: {after}\nup stderr: {}",
        String::from_utf8_lossy(&up.stderr)
    );
}

/// A live pass past its idle cap is stopped by `ostrom up`; the `pass-ended`
/// it never wrote says `failed`, `stalled`, recorded by the reaper, and its
/// cost is the run's declared cost ceiling.
#[test]
fn up_reaps_a_stalled_pass_and_records_its_pass_ended() {
    let home = Home::new();
    let mut sleeper = Sleeper::start();
    home.pass_hold(Some(&sleeper), &json!({"costUsd": 3.5, "idleMs": 1_000}));
    // The run's last event is its `run.started`, stamped now.
    thread::sleep(Duration::from_secs(3));

    let output = home.up();
    let stopped = sleeper.wait_stopped(Duration::from_secs(15));

    let ended = home.terminal_rows("pass-ended", "owner", PASS_OWNER);
    let row = ended.first().cloned().unwrap_or(Value::Null);
    assert_eq!(
        json!({
            "up_succeeded": output.status.success(),
            "process_stopped": stopped,
            "terminal_rows": ended.len(),
            "outcome": row["fact"]["outcome"],
            "reason": row["fact"]["reason"],
            "recorded_by": row["fact"]["recorded_by"],
            "cost_usd": row["fact"]["cost_usd"],
        }),
        json!({
            "up_succeeded": true,
            "process_stopped": true,
            "terminal_rows": 1,
            "outcome": "failed",
            "reason": "stalled",
            "recorded_by": "reaper",
            "cost_usd": 3.5,
        }),
        "up stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A pass whose process is gone with no terminal row (systemd stopped it at
/// the unit timeout) is closed by `ostrom up` with `exited-without-terminal`.
#[test]
fn up_closes_a_pass_whose_process_is_gone_without_a_terminal_row() {
    let home = Home::new();
    home.pass_hold(None, &json!({"costUsd": 2.25, "wallMs": 1_800_000}));

    let output = home.up();

    let ended = home.terminal_rows("pass-ended", "owner", PASS_OWNER);
    let row = ended.first().cloned().unwrap_or(Value::Null);
    assert_eq!(
        json!({
            "up_succeeded": output.status.success(),
            "terminal_rows": ended.len(),
            "outcome": row["fact"]["outcome"],
            "reason": row["fact"]["reason"],
            "recorded_by": row["fact"]["recorded_by"],
            "cost_usd": row["fact"]["cost_usd"],
            "cost_basis": row["fact"]["cost_basis"],
        }),
        json!({
            "up_succeeded": true,
            "terminal_rows": 1,
            "outcome": "failed",
            "reason": "exited-without-terminal",
            "recorded_by": "reaper",
            "cost_usd": 2.25,
            "cost_basis": "declared-ceiling",
        }),
        "up stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A reaped pass that declared no cost ceiling is charged the shared per-run
/// default, the one a work order is created with, and says so. It is never
/// charged the daily cap, which would hold every other pass for the day.
#[test]
fn a_reaped_pass_with_no_declared_ceiling_is_charged_the_per_run_default() {
    let home = Home::new();
    let mut sleeper = Sleeper::start();
    home.pass_hold(Some(&sleeper), &json!({"idleMs": 1_000}));
    thread::sleep(Duration::from_secs(3));

    let output = home
        .command()
        .env("MANDATE_DAILY_CAP_USD", "40")
        .arg("up")
        .output()
        .expect("run ostrom up");
    let stopped = sleeper.wait_stopped(Duration::from_secs(15));

    let ended = home.terminal_rows("pass-ended", "owner", PASS_OWNER);
    let row = ended.first().cloned().unwrap_or(Value::Null);
    assert_eq!(
        json!({
            "up_succeeded": output.status.success(),
            "process_stopped": stopped,
            "reason": row["fact"]["reason"],
            "cost_usd": row["fact"]["cost_usd"].as_f64(),
            "cost_basis": row["fact"]["cost_basis"],
        }),
        json!({
            "up_succeeded": true,
            "process_stopped": true,
            "reason": "stalled",
            "cost_usd": ostrom_core::DEFAULT_RUN_COST_CEILING_USD,
            "cost_basis": "default-ceiling",
        }),
        "up stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// #619 change 4: a slot that comes due while the previous worker for the
/// loop is still running is recorded as `skipped:previous-live` and nothing
/// is launched.
#[test]
fn up_with_a_live_previous_worker_records_the_skip_and_launches_nothing() {
    let marker_root = TempDir::new().expect("marker directory");
    let marker = marker_root.path().join("operation-ran");
    let (home, digest) = Home::composed(&format!(
        r#"manifest_version: 1
actors: {{builder: {{}}}}
operations:
  scheduled-work:
    steps:
      - uses: cmd/run
        with:
          script: 'touch "{}"'
loops:
  builder-day:
    actor: builder
    operation: scheduled-work
    repositories: placeholder-org/repository
    every: hourly
grants:
  scheduled: {{actors: builder, operations: scheduled-work, repositories: placeholder-org/repository}}
"#,
        marker.display()
    ));
    let mut worker = Sleeper::start();
    let (_, start_time) = worker.identity();
    fs::create_dir_all(home.path.join("loop-runs")).expect("create loop-runs");
    fs::write(
        home.path.join("loop-runs/builder-day.json"),
        json!({
            "schema_version": 1,
            "name": "builder-day",
            "version": digest,
            "schedule_slot": "prior-slot",
            "status": "running",
            "pid": worker.pid(),
            "process_start_time": start_time,
            "started_at": timestamp(epoch_now() - 600),
            "finished_at": null,
            "reason": null,
        })
        .to_string(),
    )
    .expect("write the running worker's state");

    let output = home.up();
    thread::sleep(Duration::from_millis(500));
    let state: Value = serde_json::from_slice(
        &fs::read(home.path.join("loop-runs/builder-day.json")).expect("read loop state"),
    )
    .expect("loop state JSON");
    let log = fs::read_to_string(home.path.join("loop-runs/builder-day.log")).unwrap_or_default();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    assert_eq!(
        json!({
            "up_succeeded": output.status.success(),
            "skipped": stdout.contains("skipped=1"),
            "started": stdout.contains("started=0"),
            "status": state["status"],
            "carries_the_live_pid": state["pid"] == worker.pid(),
            "logged": log.contains("skipped:previous-live"),
            "operation_ran": marker.exists(),
            "previous_worker_running": worker.running(),
        }),
        json!({
            "up_succeeded": true,
            "skipped": true,
            "started": true,
            "status": "skipped:previous-live",
            "carries_the_live_pid": true,
            "logged": true,
            "operation_ran": false,
            "previous_worker_running": true,
        }),
        "up stdout: {stdout}\nup stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// #635 S3, with `SIGKILL` escalation: two reapers race on one stalled hold
/// whose process ignores `SIGTERM`. The claim on the run lets exactly one of
/// them act: one stop, escalated to `SIGKILL` after the grace, one
/// `work-failed`, one charge.
#[test]
fn two_reapers_racing_on_one_hold_record_one_row_and_one_charge() {
    let home = Home::new();
    let mut target = Sleeper::ignoring_term();
    home.implementer_hold(&target, 1, 0);

    let started = Instant::now();
    let reapers = [(); 2].map(|()| {
        home.command()
            .arg("up")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start a reaper")
    });
    let outputs = reapers.map(|reaper| reaper.wait_with_output().expect("wait for a reaper"));
    let elapsed = started.elapsed();
    let stopped = target.wait_stopped(Duration::from_secs(5));

    let failed = home.terminal_rows("work-failed", "order_id", ORDER);
    let charged = failed
        .iter()
        .filter_map(|row| row["fact"]["cost_usd"].as_f64())
        .sum::<f64>();
    assert_eq!(
        json!({
            "both_succeeded": outputs.iter().all(|output| output.status.success()),
            "reapers_that_reaped": outputs.iter().map(reaped).sum::<u64>(),
            "terminal_rows": failed.len(),
            "charged": charged,
            "process_stopped": stopped,
            "escalated_after_the_term_grace": elapsed
                >= Duration::from_secs(ostrom_core::RUN_TERMINATION_GRACE_SECONDS * 2),
            "claim_removed": !home.claim().exists(),
            "lease_released": !home.item_lease().exists(),
        }),
        json!({
            "both_succeeded": true,
            "reapers_that_reaped": 1,
            "terminal_rows": 1,
            "charged": 20.0,
            "process_stopped": true,
            "escalated_after_the_term_grace": true,
            "claim_removed": true,
            "lease_released": true,
        }),
        "stderr: {} | {}",
        String::from_utf8_lossy(&outputs[0].stderr),
        String::from_utf8_lossy(&outputs[1].stderr)
    );
}

/// #635: a reaper claimed a run and stopped it, then died before recording
/// anything. The next reaper takes its claim over and completes it with what
/// the claim says, once: a later reaper finds nothing left to do.
#[test]
fn a_claim_left_by_a_reaper_that_died_is_completed_once_by_the_next_reaper() {
    let home = Home::new();
    let mut sleeper = Sleeper::start();
    home.implementer_hold(&sleeper, 1, 0);
    let (reaper_pid, reaper_start_time) = dead_process();
    fs::create_dir_all(home.path.join("reaping")).expect("create the claim directory");
    fs::write(
        home.claim(),
        json!({
            "run_id": RUN,
            "kind": "implementer",
            "order_id": ORDER,
            "reason": "stalled",
            "cost_usd": 20.0,
            "cost_basis": "declared-ceiling",
            "claimed_at": timestamp(epoch_now() - 5),
            "last_progress_at": timestamp(epoch_now() - 60),
            "stalled_seconds": 42,
            "reaper_pid": reaper_pid,
            "reaper_start_time": reaper_start_time,
        })
        .to_string(),
    )
    .expect("leave the dead reaper's claim");
    // Its stop landed before it died.
    sleeper.kill();

    let first = home.up();
    let second = home.up();

    let failed = home.terminal_rows("work-failed", "order_id", ORDER);
    let row = failed.first().cloned().unwrap_or(Value::Null);
    assert_eq!(
        json!({
            "up_succeeded": first.status.success() && second.status.success(),
            "first_reaped": reaped(&first),
            "second_reaped": reaped(&second),
            "terminal_rows": failed.len(),
            "reason": row["fact"]["reason"],
            "stalled_seconds_from_the_claim": row["fact"]["stalled_seconds"],
            "cost_usd": row["fact"]["cost_usd"].as_f64(),
            "cost_basis": row["fact"]["cost_basis"],
            "claim_removed": !home.claim().exists(),
            "lease_released": !home.item_lease().exists(),
        }),
        json!({
            "up_succeeded": true,
            "first_reaped": 1,
            "second_reaped": 0,
            "terminal_rows": 1,
            "reason": "stalled",
            "stalled_seconds_from_the_claim": 42,
            "cost_usd": 20.0,
            "cost_basis": "declared-ceiling",
            "claim_removed": true,
            "lease_released": true,
        }),
        "up stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
}

/// #635 S1: a stop whose outcome cannot be read is not a stop. Here the
/// service manager reports the unit live, accepts the stop, then will not say
/// what the unit's state is. Nothing is recorded, nothing is released, the
/// claim stays for the next reaper, and doctor says so.
#[test]
fn a_stop_that_cannot_be_confirmed_records_nothing_releases_nothing_and_keeps_the_claim() {
    let home = Home::new();
    home.unit_hold(1);
    let calls = home.root.path().join("systemctl-show-calls");
    let systemctl = home.root.path().join("systemctl-stub");
    fs::write(
        &systemctl,
        format!(
            concat!(
                "#!/bin/sh\n",
                "case \"$*\" in\n",
                "  *' show '*)\n",
                "    printf '%s\\n' show >>'{calls}'\n",
                "    if [ \"$(wc -l <'{calls}')\" -eq 1 ]; then printf '%s\\n' ActiveState=active; exit 0; fi\n",
                "    exit 1 ;;\n",
                "esac\n",
                "exit 0\n"
            ),
            calls = calls.display()
        ),
    )
    .expect("write the systemctl stub");
    fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755))
        .expect("make the systemctl stub executable");

    let output = home
        .command()
        .env("MANDATE_SYSTEMCTL_BIN", &systemctl)
        .arg("up")
        .output()
        .expect("run ostrom up");
    let doctor = home.doctor_work_orders();

    let claim = fs::read(home.claim())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(Value::Null);
    assert_eq!(
        json!({
            "up_succeeded": output.status.success(),
            "says_the_stop_is_unconfirmed": String::from_utf8_lossy(&output.stderr)
                .contains("stop unconfirmed"),
            "counted_as_reaped": reaped(&output),
            "terminal_rows": home.terminal_rows("work-failed", "order_id", ORDER).len(),
            "lease_held": home.item_lease().exists(),
            "claim_kept_with_its_reason": claim["reason"],
            "doctor_fails_on_the_kept_claim": doctor.starts_with("FAIL|work-orders|stall reaper:")
                && doctor.contains(&format!("run={RUN}")),
        }),
        json!({
            "up_succeeded": true,
            "says_the_stop_is_unconfirmed": true,
            "counted_as_reaped": 0,
            "terminal_rows": 0,
            "lease_held": true,
            "claim_kept_with_its_reason": "stalled",
            "doctor_fails_on_the_kept_claim": true,
        }),
        "up stderr: {}\ndoctor: {doctor}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// #635 S6: reaping is best effort and scheduling is not. A reaper that fails
/// (here, on a trace it cannot read) is printed and recorded for doctor, and
/// `ostrom up` still launches the loop that is due.
#[test]
fn a_reaper_error_is_recorded_for_doctor_and_does_not_stop_a_launch() {
    let marker_root = TempDir::new().expect("marker directory");
    let marker = marker_root.path().join("operation-ran");
    let (home, _digest) = Home::composed(&format!(
        r#"manifest_version: 1
actors: {{builder: {{}}}}
operations:
  scheduled-work:
    steps:
      - uses: cmd/run
        with:
          script: 'touch "{}"'
loops:
  builder-day:
    actor: builder
    operation: scheduled-work
    repositories: placeholder-org/repository
    every: hourly
grants:
  scheduled: {{actors: builder, operations: scheduled-work, repositories: placeholder-org/repository}}
"#,
        marker.display()
    ));
    // A persistent fault the reaper cannot get past: the trace is unreadable.
    fs::create_dir_all(home.path.join("sprint.jsonl")).expect("make the trace unreadable");

    let output = home.up();
    let doctor = home.doctor_work_orders();
    let state = fs::read(home.path.join("loop-runs/builder-day.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(Value::Null);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert_eq!(
        json!({
            "up_succeeded": output.status.success(),
            "launched": stdout.contains("started=1"),
            "loop_state_written": state["status"].is_string(),
            "reaper_error_printed": String::from_utf8_lossy(&output.stderr)
                .contains("ostrom up: stall reaper:"),
            "doctor_names_the_reaper_error": doctor.starts_with("FAIL|work-orders|stall reaper:")
                && doctor.contains("the stall reaper failed"),
        }),
        json!({
            "up_succeeded": true,
            "launched": true,
            "loop_state_written": true,
            "reaper_error_printed": true,
            "doctor_names_the_reaper_error": true,
        }),
        "up stdout: {stdout}\nup stderr: {}\ndoctor: {doctor}",
        String::from_utf8_lossy(&output.stderr)
    );
}
