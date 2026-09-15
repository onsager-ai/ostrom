#![cfg(unix)]

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use ostrom_core::WorkOrder;
use serde_json::{Value, json};
use tempfile::TempDir;

const PROXY_VARIABLES: &[(&str, &str)] = &[
    ("ALL_PROXY", "upper-all-placeholder"),
    ("HTTPS_PROXY", "upper-secure-placeholder"),
    ("HTTP_PROXY", "upper-plain-placeholder"),
    ("NO_PROXY", "upper-bypass-placeholder"),
    ("all_proxy", "lower-all-placeholder"),
    ("https_proxy", "lower-secure-placeholder"),
    ("http_proxy", "lower-plain-placeholder"),
    ("no_proxy", "lower-bypass-placeholder"),
];

struct DispatchFixture {
    root: TempDir,
    home: PathBuf,
    state: PathBuf,
    source: PathBuf,
    order_file: PathBuf,
    item_hash: String,
    codex: PathBuf,
    gh_as: PathBuf,
    systemd_run: PathBuf,
    systemd_args: PathBuf,
}

impl DispatchFixture {
    fn new(explicit_config: bool) -> Self {
        let root = tempfile::tempdir().expect("temporary dispatch fixture");
        let home = root.path().join("home");
        let state = if explicit_config {
            root.path().join("config/ostrom")
        } else {
            home.join(".claude/ostrom")
        };
        let source = root.path().join("placeholder-source");
        fs::create_dir_all(&state).expect("create state root");
        fs::create_dir_all(&source).expect("create source repository placeholder");

        let order = json!({
            "schema_version": 1,
            "item_id": "placeholder-org/alpha#7",
            "repository": "placeholder-org/alpha",
            "item_ref": "#7",
            "branch_name": "ostrom/7-placeholder",
            "spec": "Change a placeholder fixture.",
            "acceptance_criteria": ["The placeholder changes."],
            "constraints": ["Use placeholder data only."],
            "order_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "created_at": "2026-08-01T00:00:00Z",
            "cost_ceiling_usd": 20,
            "token_ceiling": 500000
        });
        let order_file = root.path().join("work-order.json");
        fs::write(
            &order_file,
            format!(
                "{}\n",
                serde_json::to_string(&order).expect("serialize work order")
            ),
        )
        .expect("write work order");
        let parsed = WorkOrder::from_json(&fs::read(&order_file).expect("read work order"))
            .expect("valid work order");

        let codex = root.path().join("codex-stub");
        executable(&codex, "exit 0");
        let gh_as = root.path().join("credential-stub");
        executable(
            &gh_as,
            concat!(
                "if printf '%s\\n' \"$*\" | grep -Fq '/branches?'; then\n",
                "  printf '%s\\n' '[{\"name\":\"main\",\"commit\":{\"sha\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}}]'\n",
                "elif printf '%s\\n' \"$*\" | grep -Fq ' issue view '; then\n",
                "  printf '%s\\n' '{\"closedByPullRequestsReferences\":[]}'\n",
                "elif printf '%s\\n' \"$*\" | grep -Fq ' pr list '; then\n",
                "  printf '%s\\n' '[]'\n",
                "else\n",
                "  exit 1\n",
                "fi"
            ),
        );
        let systemd_args = root.path().join("systemd-args");
        let systemd_run = root.path().join("systemd-run-stub");
        executable(
            &systemd_run,
            "printf '%s\\n' \"$@\" >\"$FAKE_SYSTEMD_ARGS\"",
        );

        Self {
            root,
            home,
            state,
            source,
            order_file,
            item_hash: parsed.item_hash(),
            codex,
            gh_as,
            systemd_run,
            systemd_args,
        }
    }

    fn dispatch(&self, explicit_config: bool) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        command
            .arg("dispatch")
            .arg(&self.order_file)
            .current_dir(self.root.path())
            .env_remove("OSTROM_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env("HOME", &self.home)
            .env("OSTROM_PLUGIN_ROOT", plugin_root())
            .env("MANDATE_IMPLEMENTER_SOURCE_REPO", &self.source)
            .env("MANDATE_GH_AS_BIN", &self.gh_as)
            .env("MANDATE_SYSTEMD_RUN_BIN", &self.systemd_run)
            .env("MANDATE_OSTROM_BIN", env!("CARGO_BIN_EXE_ostrom"))
            .env("CODEX_BIN", &self.codex)
            .env("FAKE_SYSTEMD_ARGS", &self.systemd_args);
        if explicit_config {
            command.env("CLAUDE_CONFIG_DIR", self.root.path().join("config"));
        }
        command
    }

    fn assert_child_resolves_parent_state(&self, dispatch: Output) {
        assert!(
            dispatch.status.success(),
            "{}",
            String::from_utf8_lossy(&dispatch.stderr)
        );
        let unit = String::from_utf8(dispatch.stdout)
            .expect("dispatch stdout is UTF-8")
            .trim()
            .to_owned();
        let lease_file = self
            .state
            .join(format!("implementer-item-{}.lease", self.item_hash));
        let parent_lease: Value = serde_json::from_slice(
            &fs::read(&lease_file).expect("dispatcher created lease in parent state root"),
        )
        .expect("parent lease JSON");
        assert_eq!(parent_lease["owner"], unit);
        assert_eq!(
            parent_lease["expires_at"].as_u64().unwrap()
                - parent_lease["started_at"].as_u64().unwrap(),
            5_300,
            "500,000 weighted tokens at 100/s dominates $20 at 240s/$, then adds 5m"
        );

        let child_environment = captured_environment(&self.systemd_args);
        let mut child = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        child
            .args(["lease", "status"])
            .current_dir(self.root.path())
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default());
        for (name, value) in child_environment {
            child.env(name, value);
        }
        let child = child
            .output()
            .expect("resolve state in captured child environment");
        assert!(
            child.status.success(),
            "child did not resolve the dispatcher's state root: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let child_lease: Value = serde_json::from_slice(&child.stdout).expect("child lease JSON");
        assert_eq!(child_lease, parent_lease);
    }
}

#[test]
fn dispatch_child_resolves_legacy_home_state_without_config_overrides() {
    let fixture = DispatchFixture::new(false);
    let dispatch = fixture
        .dispatch(false)
        .output()
        .expect("dispatch through systemd stub");
    fixture.assert_child_resolves_parent_state(dispatch);
}

#[test]
fn dispatch_child_resolves_explicit_claude_config_state() {
    let fixture = DispatchFixture::new(true);
    let dispatch = fixture
        .dispatch(true)
        .output()
        .expect("dispatch through systemd stub");
    fixture.assert_child_resolves_parent_state(dispatch);
}

#[test]
fn dispatch_reports_each_orphan_worktree_removal() {
    let fixture = DispatchFixture::new(false);
    let orphan = fixture
        .state
        .join("implementer-worktrees/orphan-placeholder");
    fs::create_dir_all(&orphan).expect("create orphan worktree");
    fs::write(orphan.join("artifact"), "placeholder").expect("write orphan artifact");

    let output = fixture
        .dispatch(false)
        .output()
        .expect("dispatch with orphan worktree");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("dispatch stderr is UTF-8");
    assert!(
        stderr.contains("removed orphan implementer worktree"),
        "{stderr}"
    );
    assert!(
        stderr.contains(orphan.to_str().expect("UTF-8 orphan path")),
        "{stderr}"
    );
    assert!(!orphan.exists());
}

#[test]
fn immediately_dead_unit_is_not_recorded_as_dispatched() {
    let fixture = DispatchFixture::new(false);
    let systemctl = fixture.root.path().join("systemctl-stub");
    executable(&systemctl, "exit 1");
    let output = fixture
        .dispatch(false)
        .env("MANDATE_SYSTEMCTL_BIN", systemctl)
        .env("MANDATE_IMPLEMENTER_STARTUP_GRACE_MILLISECONDS", "0")
        .output()
        .expect("dispatch with dead unit");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("exited during startup"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let trace = fs::read_to_string(fixture.state.join("sprint.jsonl")).expect("failure trace");
    let rows = trace
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("trace row"))
        .collect::<Vec<_>>();
    assert!(!rows.iter().any(|row| row["kind"] == "work-dispatched"));
    assert!(rows.iter().any(|row| {
        row["kind"] == "work-failed" && row["fact"]["reason"] == "dispatch-startup-failed"
    }));
    assert!(
        !fixture
            .state
            .join(format!("implementer-item-{}.lease", fixture.item_hash))
            .exists()
    );
}

#[test]
fn invalid_order_after_lease_adoption_releases_the_lease() {
    let fixture = tempfile::tempdir().expect("temporary implementer startup fixture");
    let state = fixture.path().join("state");
    fs::create_dir(&state).expect("create state");
    let order = fixture.path().join("invalid-order.json");
    fs::write(&order, "{}\n").expect("write invalid order");
    let lease_name =
        "implementer-item-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.lease";
    let unit = "ostrom-implementer-aaaaaaaaaaaaaaaa";
    fs::write(
        state.join(lease_name),
        format!("{{\"owner\":\"{unit}\",\"started_at\":1,\"expires_at\":9999999999}}\n"),
    )
    .expect("write dispatch-owned lease");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["implement", order.to_str().unwrap(), unit])
        .env("OSTROM_HOME", &state)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env("MANDATE_LEASE_NAME", lease_name)
        .output()
        .expect("run implementer with invalid order");
    assert_eq!(output.status.code(), Some(2));
    assert!(!state.join(lease_name).exists());
    let run_directories = fs::read_dir(state.join("runs"))
        .expect("read failed implementer runs")
        .collect::<Result<Vec<_>, _>>()
        .expect("read failed implementer run entries");
    assert_eq!(run_directories.len(), 1);
    let events = fs::read_to_string(run_directories[0].path().join("events.jsonl"))
        .expect("read failed implementer events")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("event JSON"))
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["type"], "run.started");
    assert_eq!(events[1]["type"], "run.finished");
    assert_eq!(events[1]["payload"]["outcome"], "failed");
}

#[test]
fn process_backend_launches_a_detached_session_with_the_systemd_environment() {
    let fixture = DispatchFixture::new(false);
    let worker_calls = fixture.root.path().join("worker.calls");
    let worker_environment = fixture.root.path().join("worker.env");
    let worker = fixture.root.path().join("implementer-stub");
    executable(
        &worker,
        &format!(
            "printf '%s\\n' called >>'{}'\nenv >'{}'\nprintf '%s\\n' process-stdout\nprintf '%s\\n' process-stderr >&2\ni=0; while [ \"$i\" -lt 30 ]; do sleep 1; i=$((i + 1)); done",
            worker_calls.display(),
            worker_environment.display()
        ),
    );
    let seam_calls = fixture.root.path().join("manager.calls");
    let systemd_run = failing_seam(&fixture.root, "systemd-run-fail", &seam_calls);
    let systemctl = failing_seam(&fixture.root, "systemctl-fail", &seam_calls);
    let mut command = fixture.dispatch(false);
    command
        .env("MANDATE_DISPATCH_BACKEND", "process")
        .env("MANDATE_OSTROM_BIN", &worker)
        .env("MANDATE_SYSTEMD_RUN_BIN", systemd_run)
        .env("MANDATE_SYSTEMCTL_BIN", systemctl)
        .env("OSTROM_TEST_DISPATCHER_SECRET", "sentinel");
    for (name, value) in PROXY_VARIABLES {
        command.env(name, value);
    }
    let output = command.output().expect("dispatch through process backend");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let lease = process_lease(&fixture);
    let pid = lease["pid"].as_u64().expect("lease pid") as u32;
    let process_group = lease["process_group_id"]
        .as_u64()
        .expect("lease process group") as u32;
    let _guard = KillProcessGroup(process_group);
    assert_eq!(pid, process_group);
    assert!(lease["process_start_time"].as_u64().unwrap() > 0);
    let identity = proc_identity(pid).expect("live process identity");
    assert_eq!(identity.0, process_group);
    assert_eq!(identity.1, pid, "the process must lead a new session");
    assert!(!seam_calls.exists(), "service-manager seams were invoked");
    assert_eq!(fs::read_to_string(&worker_calls).unwrap(), "called\n");
    let environment = fs::read_to_string(worker_environment).expect("captured environment");
    for expected in [
        format!("OSTROM_HOME={}", fixture.state.display()),
        format!("HOME={}", fixture.home.display()),
        format!("OSTROM_PLUGIN_ROOT={}", plugin_root().display()),
        format!(
            "MANDATE_IMPLEMENTER_SOURCE_REPO={}",
            fixture.source.display()
        ),
        "MANDATE_DAILY_CAP_USD=50".to_owned(),
        "MANDATE_MAX_IMPLEMENTERS=2".to_owned(),
        "MANDATE_MAX_IMPLEMENTERS_PER_REPOSITORY=1".to_owned(),
        "MANDATE_DISPATCH_BACKEND=process".to_owned(),
        format!(
            "MANDATE_LEASE_NAME=implementer-item-{}.lease",
            fixture.item_hash
        ),
    ] {
        assert!(
            environment.lines().any(|line| line == expected),
            "{environment}"
        );
    }
    for (name, value) in PROXY_VARIABLES {
        let expected = format!("{name}={value}");
        assert!(
            environment.lines().any(|line| line == expected),
            "missing process proxy variable {name}: {environment}"
        );
    }
    for absent in [
        "CLAUDE_CONFIG_DIR=",
        "MANDATE_GH_AS_BIN=",
        "OSTROM_TEST_DISPATCHER_SECRET=",
    ] {
        assert!(
            !environment.lines().any(|line| line.starts_with(absent)),
            "unexpected inherited variable {absent}: {environment}"
        );
    }
    assert!(environment.lines().any(|line| line.starts_with("PATH=")));
    let log = fixture
        .state
        .join(format!("implementer-item-{}.log", fixture.item_hash));
    let log_contents = fs::read_to_string(&log).expect("implementer log");
    assert!(log_contents.contains("process-stdout"), "{log_contents}");
    assert!(log_contents.contains("process-stderr"), "{log_contents}");
    assert_eq!(
        fs::metadata(log).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let trace = trace(&fixture.state);
    let dispatched = trace
        .iter()
        .find(|row| row["kind"] == "work-dispatched")
        .expect("work-dispatched row");
    assert_eq!(dispatched["fact"]["backend"], "process");
}

#[test]
fn process_backend_omits_unset_proxy_variables_and_other_ambient_values() {
    let fixture = DispatchFixture::new(false);
    let worker_environment = fixture.root.path().join("unset-proxy-worker.env");
    let worker = fixture.root.path().join("unset-proxy-implementer-stub");
    executable(
        &worker,
        &format!(
            "env >'{}'\ni=0; while [ \"$i\" -lt 30 ]; do sleep 1; i=$((i + 1)); done",
            worker_environment.display()
        ),
    );
    let mut command = fixture.dispatch(false);
    command
        .env("MANDATE_DISPATCH_BACKEND", "process")
        .env("MANDATE_OSTROM_BIN", &worker)
        .env("OSTROM_TEST_DISPATCHER_SECRET", "sentinel");
    for (name, _) in PROXY_VARIABLES {
        command.env_remove(name);
    }

    let output = command.output().expect("dispatch without proxy variables");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lease = process_lease(&fixture);
    let process_group = lease["process_group_id"]
        .as_u64()
        .expect("lease process group") as u32;
    let _guard = KillProcessGroup(process_group);
    let environment = fs::read_to_string(worker_environment).expect("captured worker environment");
    for (name, _) in PROXY_VARIABLES {
        assert!(
            !environment
                .lines()
                .any(|line| line.starts_with(&format!("{name}="))),
            "unexpected process proxy variable {name}: {environment}"
        );
    }
    assert!(
        !environment
            .lines()
            .any(|line| line.starts_with("OSTROM_TEST_DISPATCHER_SECRET=")),
        "non-allowlisted dispatcher value reached the implementer: {environment}"
    );
}

#[test]
fn killing_the_dispatcher_does_not_kill_or_duplicate_the_process_implementer() {
    let fixture = DispatchFixture::new(false);
    let worker_calls = fixture.root.path().join("durable-worker.calls");
    let worker = fixture.root.path().join("durable-implementer-stub");
    executable(
        &worker,
        &format!(
            "printf '%s\\n' called >>'{}'\ni=0; while [ \"$i\" -lt 30 ]; do sleep 1; i=$((i + 1)); done",
            worker_calls.display()
        ),
    );
    let seam_calls = fixture.root.path().join("durable-manager.calls");
    let systemd_run = failing_seam(&fixture.root, "durable-systemd-run-fail", &seam_calls);
    let systemctl = failing_seam(&fixture.root, "durable-systemctl-fail", &seam_calls);
    let configure = |command: &mut Command| {
        command
            .env("MANDATE_DISPATCH_BACKEND", "process")
            .env("MANDATE_OSTROM_BIN", &worker)
            .env("MANDATE_SYSTEMD_RUN_BIN", &systemd_run)
            .env("MANDATE_SYSTEMCTL_BIN", &systemctl)
            .env("MANDATE_IMPLEMENTER_STARTUP_GRACE_MILLISECONDS", "30000");
    };
    let mut first = fixture.dispatch(false);
    configure(&mut first);
    let mut dispatcher = first
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dispatching process");
    wait_until(Duration::from_secs(5), || {
        fs::read(
            fixture
                .state
                .join(format!("implementer-item-{}.lease", fixture.item_hash)),
        )
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|lease| lease["pid"].is_u64())
    });
    let lease = process_lease(&fixture);
    let pid = lease["pid"].as_u64().unwrap() as u32;
    let process_group = lease["process_group_id"].as_u64().unwrap() as u32;
    let start_time = lease["process_start_time"].as_u64().unwrap();
    let _guard = KillProcessGroup(process_group);
    kill_pid(dispatcher.id(), "KILL");
    assert!(!dispatcher.wait().unwrap().success());
    assert_eq!(
        proc_identity(pid).map(|identity| identity.2),
        Some(start_time)
    );

    let mut second = fixture.dispatch(false);
    configure(&mut second);
    let output = second.output().expect("try duplicate dispatch");
    assert_eq!(output.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("item already has a live implementer lease")
    );
    assert_eq!(fs::read_to_string(worker_calls).unwrap(), "called\n");
    assert!(!seam_calls.exists(), "service-manager seams were invoked");
}

#[test]
fn process_startup_failure_escalates_to_the_stubborn_grandchild() {
    let fixture = DispatchFixture::new(false);
    let group_file = fixture.root.path().join("failed-process.group");
    let grandchild_file = fixture.root.path().join("failed-process.grandchild");
    let term_seen = fixture.root.path().join("failed-process.term");
    let worker = fixture.root.path().join("exiting-implementer-stub");
    executable(
        &worker,
        &format!(
            "printf '%s\\n' \"$BASHPID\" >'{}'\n( set +e; trap ': >\"{}\"' TERM; i=0; while [ \"$i\" -lt 30 ]; do sleep 1; i=$((i + 1)); done ) &\nprintf '%s\\n' \"$!\" >'{}'\nsleep 0.1\nexit 0",
            group_file.display(),
            term_seen.display(),
            grandchild_file.display()
        ),
    );
    let output = fixture
        .dispatch(false)
        .env("MANDATE_DISPATCH_BACKEND", "process")
        .env("MANDATE_OSTROM_BIN", &worker)
        .env("MANDATE_IMPLEMENTER_STARTUP_GRACE_MILLISECONDS", "250")
        .env("MANDATE_IMPLEMENTER_TERMINATION_GRACE_SECONDS", "1")
        .output()
        .expect("dispatch short-lived process");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("exited during startup"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let process_group = fs::read_to_string(group_file)
        .expect("process group")
        .trim()
        .parse::<u32>()
        .expect("numeric process group");
    let grandchild = fs::read_to_string(grandchild_file)
        .expect("grandchild pid")
        .trim()
        .parse::<u32>()
        .expect("numeric grandchild pid");
    let _guard = KillProcessGroup(process_group);
    assert!(term_seen.exists(), "the group did not receive TERM");
    wait_until(Duration::from_secs(5), || {
        !process_group_alive(process_group)
    });
    assert!(!process_group_alive(process_group));
    assert!(!pid_alive(grandchild));
    let failed = trace(&fixture.state)
        .into_iter()
        .find(|row| row["fact"]["reason"] == "dispatch-startup-failed")
        .expect("startup failure row");
    assert_eq!(failed["fact"]["backend"], "process");
}

fn captured_environment(path: &Path) -> BTreeMap<String, String> {
    let arguments = fs::read_to_string(path).expect("read captured systemd arguments");
    let mut environment = BTreeMap::new();
    let mut lines = arguments.lines();
    while let Some(argument) = lines.next() {
        if argument == "--setenv" {
            let assignment = lines.next().expect("--setenv value");
            let (name, value) = assignment.split_once('=').expect("environment assignment");
            environment.insert(name.to_owned(), value.to_owned());
        }
    }
    environment
}

fn process_lease(fixture: &DispatchFixture) -> Value {
    serde_json::from_slice(
        &fs::read(
            fixture
                .state
                .join(format!("implementer-item-{}.lease", fixture.item_hash)),
        )
        .expect("process lease"),
    )
    .expect("process lease JSON")
}

fn trace(state: &Path) -> Vec<Value> {
    fs::read_to_string(state.join("sprint.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("trace row"))
        .collect()
}

fn proc_identity(pid: u32) -> Option<(u32, u32, u64)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    Some((
        fields.get(2)?.parse().ok()?,
        fields.get(3)?.parse().ok()?,
        fields.get(19)?.parse().ok()?,
    ))
}

fn failing_seam(root: &TempDir, name: &str, calls: &Path) -> PathBuf {
    let path = root.path().join(name);
    executable(
        &path,
        &format!("printf '%s\\n' called >>'{}'; exit 97", calls.display()),
    );
    path
}

fn wait_until(ceiling: Duration, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + ceiling;
    while !condition() {
        assert!(Instant::now() < deadline, "condition was not reached");
        thread::sleep(Duration::from_millis(20));
    }
}

fn kill_pid(pid: u32, signal: &str) {
    let status = Command::new("/bin/kill")
        .args([format!("-{signal}"), pid.to_string()])
        .status()
        .expect("send signal");
    assert!(status.success());
}

fn pid_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn process_group_alive(process_group: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", "--", &format!("-{process_group}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

struct KillProcessGroup(u32);

impl Drop for KillProcessGroup {
    fn drop(&mut self) {
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", self.0)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn plugin_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../ostrom-store/assets")
        .canonicalize()
        .expect("plugin root")
}

fn executable(path: &Path, body: &str) {
    fs::write(path, format!("#!/usr/bin/env bash\nset -eu\n{body}\n")).expect("write stub");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
}
