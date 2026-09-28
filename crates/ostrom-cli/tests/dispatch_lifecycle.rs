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

mod support;

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
    // #618: the process backend hands the implementer the same ids the hold
    // records, compared with the record rather than pinned.
    let run_id = dispatched["fact"]["run_id"]
        .as_str()
        .expect("work-dispatched names its run id");
    let order_id = dispatched["fact"]["order_id"]
        .as_str()
        .expect("work-dispatched names its order");
    for expected in [
        format!("OSTROM_RUN_ID={run_id}"),
        format!("OSTROM_WORK_ORDER_ID={order_id}"),
    ] {
        assert!(
            environment.lines().any(|line| line == expected),
            "missing {expected}: {environment}"
        );
    }
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

/// #618: dispatch mints one run id and every record of the hold carries it.
/// `work-dispatched` names it; the implementer writes its events and its
/// terminal row under it; the Codex child receives it, and the order id, in
/// its environment. Each is compared with the record dispatch wrote, never
/// pinned a second time, so a side that mints its own id fails here.
#[test]
fn dispatch_mints_the_run_id_the_implementer_and_its_harness_carry() {
    let fixture = DispatchFixture::new(false);
    let (codex_environment, credential) = runnable_implementer(&fixture);
    // A service-manager seam that runs the unit to completion, with exactly
    // the environment dispatch declared through `--setenv`.
    let systemd_run = fixture.root.path().join("systemd-run-executes");
    executable(
        &systemd_run,
        concat!(
            "while [ \"$#\" -gt 0 ]; do\n",
            "  case \"$1\" in\n",
            "    --setenv) export \"$2\"; shift 2 ;;\n",
            "    --unit|--description|--property) shift 2 ;;\n",
            "    --*) shift ;;\n",
            "    *) break ;;\n",
            "  esac\n",
            "done\n",
            "\"$@\" >>\"$FAKE_IMPLEMENTER_LOG\" 2>&1 || true"
        ),
    );
    let implementer_log = fixture.root.path().join("implementer.log");

    let output = fixture
        .dispatch(false)
        .env("MANDATE_GH_AS_BIN", &credential)
        .env("MANDATE_SYSTEMD_RUN_BIN", &systemd_run)
        .env("FAKE_IMPLEMENTER_LOG", &implementer_log)
        .env("OSTROM_RUN_ID", "builder-parent-placeholder-run")
        .output()
        .expect("dispatch through an executing service-manager seam");

    let log = fs::read_to_string(&implementer_log).unwrap_or_default();
    assert!(
        output.status.success(),
        "{}\nimplementer: {log}",
        String::from_utf8_lossy(&output.stderr)
    );
    let trace = trace(&fixture.state);
    let dispatched = trace
        .iter()
        .find(|row| row["kind"] == "work-dispatched")
        .expect("work-dispatched row");
    let recorded = dispatched["fact"]["run_id"].clone();
    let order_id = dispatched["fact"]["order_id"]
        .as_str()
        .expect("work-dispatched names its order");
    // Every observation is gathered before one comparison, so a failure shows
    // each place the id is missing or different at once instead of stopping
    // at the first.
    let implementer_events_run_id = run_started_events(&fixture.state)
        .into_iter()
        .find(|event| event["payload"]["workOrder"] == order_id)
        .map_or(Value::Null, |event| event["runId"].clone());
    let terminal_run_id = trace
        .iter()
        .find(|row| {
            matches!(row["kind"].as_str(), Some("work-completed" | "work-failed"))
                && row["fact"]["order_id"] == order_id
        })
        .map_or(Value::Null, |row| row["fact"]["run_id"].clone());
    let environment = fs::read_to_string(&codex_environment).unwrap_or_default();
    let harness = |name: &str| {
        environment
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .map_or(Value::Null, |value| json!(value))
    };
    assert_eq!(
        json!({
            "work_dispatched_names_a_run_id": recorded.is_string(),
            "runner": dispatched["fact"]["runner"],
            "parent_run_id": dispatched["fact"]["parent_run_id"],
            "implementer_events_run_id": implementer_events_run_id,
            "implementer_terminal_run_id": terminal_run_id,
            "harness_OSTROM_RUN_ID": harness("OSTROM_RUN_ID"),
            "harness_OSTROM_WORK_ORDER_ID": harness("OSTROM_WORK_ORDER_ID"),
        }),
        json!({
            "work_dispatched_names_a_run_id": true,
            "runner": "agent/codex",
            "parent_run_id": "builder-parent-placeholder-run",
            "implementer_events_run_id": recorded,
            "implementer_terminal_run_id": recorded,
            "harness_OSTROM_RUN_ID": recorded,
            "harness_OSTROM_WORK_ORDER_ID": order_id,
        }),
        "implementer log: {log}\nharness environment: {environment}"
    );
}

/// #618: `ostrom ps --json` lists an open dispatch under the run id its record
/// names, and drops it once a terminal row for that order is appended.
#[test]
fn ps_json_lists_an_open_dispatch_until_its_terminal_row() {
    let fixture = DispatchFixture::new(false);
    let output = fixture
        .dispatch(false)
        .output()
        .expect("dispatch through the recording seam");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dispatched = trace(&fixture.state)
        .into_iter()
        .find(|row| row["kind"] == "work-dispatched")
        .expect("work-dispatched row");

    let open = ps_json(&fixture.state);
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0]["kind"], "implementer");
    assert_eq!(open[0]["run_id"], dispatched["fact"]["run_id"]);
    assert_eq!(open[0]["runner"], dispatched["fact"]["runner"]);
    assert_eq!(open[0]["item"], dispatched["fact"]["item_id"]);
    assert_eq!(open[0]["order_id"], dispatched["fact"]["order_id"]);
    assert_eq!(open[0]["owner"], dispatched["fact"]["unit_name"]);
    assert_eq!(open[0]["started_at"], dispatched["ts"]);
    assert_eq!(open[0]["lease"], "live");

    // The lease column follows the lease itself: one past its expiry reads
    // `expired` while the hold stays open.
    let lease_file = fixture
        .state
        .join(format!("implementer-item-{}.lease", fixture.item_hash));
    let mut lease: Value =
        serde_json::from_slice(&fs::read(&lease_file).expect("read dispatch lease"))
            .expect("dispatch lease JSON");
    lease["started_at"] = json!(1);
    lease["expires_at"] = json!(1);
    fs::write(&lease_file, lease.to_string()).expect("expire dispatch lease");
    let expired = ps_json(&fixture.state);
    assert_eq!(expired.len(), 1, "{expired:?}");
    assert_eq!(expired[0]["lease"], "expired");

    let completed = json!({
        "ts": dispatched["ts"],
        "kind": "work-completed",
        "fact": {
            "schema_version": 1,
            "item_id": dispatched["fact"]["item_id"],
            "order_id": dispatched["fact"]["order_id"],
        },
        "narration": {},
    });
    let mut appended = fs::read_to_string(fixture.state.join("sprint.jsonl")).expect("read trace");
    appended.push_str(&format!("{completed}\n"));
    fs::write(fixture.state.join("sprint.jsonl"), appended).expect("append work-completed");

    let closed = ps_json(&fixture.state);
    assert!(closed.is_empty(), "{closed:?}");
}

/// #618 review: an `ostrom implement` run by hand inside another run (a pass's
/// agent, say) labels its harness child with its own run id and its own order,
/// never with the ids it inherited. Otherwise the hold is misattributed to the
/// enclosing run, and anything acting on that label acts on the wrong process.
#[test]
fn a_hand_run_implementer_labels_its_harness_with_its_own_run_not_an_inherited_one() {
    let fixture = DispatchFixture::new(false);
    let (codex_environment, credential) = runnable_implementer(&fixture);
    let unit = "ostrom-implementer-hand-run";
    fs::write(
        fixture
            .state
            .join(format!("implementer-item-{}.lease", fixture.item_hash)),
        format!("{{\"owner\":\"{unit}\",\"started_at\":1,\"expires_at\":9999999999}}\n"),
    )
    .expect("write the implementer lease a hand run adopts");
    let order_id = WorkOrder::from_json(&fs::read(&fixture.order_file).expect("read order"))
        .expect("valid work order")
        .order_id;

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .arg("implement")
        .arg(&fixture.order_file)
        .arg(unit)
        .current_dir(fixture.root.path())
        .env_clear()
        .env("OSTROM_HOME", &fixture.state)
        .env("HOME", &fixture.home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("OSTROM_PLUGIN_ROOT", plugin_root())
        .env("MANDATE_IMPLEMENTER_SOURCE_REPO", &fixture.source)
        .env("MANDATE_GH_AS_BIN", &credential)
        .env("CODEX_BIN", &fixture.codex)
        .env("OSTROM_RUN_ID", "builder-inherited-placeholder-run")
        .env("OSTROM_WORK_ORDER_ID", "inherited-placeholder-order")
        .output()
        .expect("run the implementer by hand");

    let run_id = run_started_events(&fixture.state)
        .into_iter()
        .find(|event| event["payload"]["workOrder"] == order_id.as_str())
        .map_or(Value::Null, |event| event["runId"].clone());
    let environment = fs::read_to_string(&codex_environment).unwrap_or_default();
    let harness = |name: &str| {
        environment
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .map_or(Value::Null, |value| json!(value))
    };
    assert_eq!(
        json!({
            "implementer_run_found": run_id.is_string(),
            "harness_OSTROM_RUN_ID": harness("OSTROM_RUN_ID"),
            "harness_OSTROM_WORK_ORDER_ID": harness("OSTROM_WORK_ORDER_ID"),
        }),
        json!({
            "implementer_run_found": true,
            "harness_OSTROM_RUN_ID": run_id,
            "harness_OSTROM_WORK_ORDER_ID": order_id,
        }),
        "implementer stderr: {}\nharness environment: {environment}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// #618 review: a `work-dispatched` row written before run ids were recorded,
/// with no `run_id` and no `runner`, is still a hold. `ps --json` lists it with
/// both as null and drops it on its terminal row.
#[test]
fn ps_json_lists_a_legacy_dispatch_row_until_its_terminal_row() {
    let root = tempfile::tempdir().expect("legacy trace fixture");
    let state = root.path();
    let dispatched = json!({
        "ts": "2026-08-01T00:00:00Z",
        "kind": "work-dispatched",
        "fact": {
            "schema_version": 1,
            "item_id": "placeholder-org/alpha#7",
            "order_id": "legacy-placeholder-order",
            "unit_name": "ostrom-implementer-legacy",
            "backend": "systemd",
            "cost_ceiling_usd": 20,
            "token_ceiling": 500000
        },
        "narration": {}
    });
    fs::write(state.join("sprint.jsonl"), format!("{dispatched}\n")).expect("write legacy trace");

    let open = ps_json(state);
    let listed = open
        .iter()
        .map(|holding| {
            json!({
                "kind": holding["kind"],
                "run_id": holding["run_id"],
                "runner": holding["runner"],
                "item": holding["item"],
                "order_id": holding["order_id"],
                "owner": holding["owner"],
                "started_at": holding["started_at"],
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        json!(listed),
        json!([{
            "kind": "implementer",
            "run_id": null,
            "runner": null,
            "item": "placeholder-org/alpha#7",
            "order_id": "legacy-placeholder-order",
            "owner": "ostrom-implementer-legacy",
            "started_at": "2026-08-01T00:00:00Z",
        }])
    );

    let completed = json!({
        "ts": "2026-08-01T01:00:00Z",
        "kind": "work-completed",
        "fact": {"schema_version": 1, "item_id": "placeholder-org/alpha#7", "order_id": "legacy-placeholder-order"},
        "narration": {}
    });
    fs::write(
        state.join("sprint.jsonl"),
        format!("{dispatched}\n{completed}\n"),
    )
    .expect("append the legacy terminal row");
    let closed = ps_json(state);
    assert!(closed.is_empty(), "{closed:?}");
}

/// #619 change 1: an implementer runs under the wall cap its operator
/// declared in `defaults.implementer_ceilings`, read by dispatch from the
/// current composed version. A Codex stub that never finishes is stopped by
/// the implementer's own watchdog at that cap, the terminal row says
/// `wall-cap`, and the unit carries the cap plus the termination grace as its
/// outer bound.
#[test]
fn an_implementer_past_its_declared_wall_cap_is_stopped_and_says_why() {
    let fixture = DispatchFixture::new(false);
    let (_codex_environment, credential) = runnable_implementer(&fixture);
    let codex_pid = fixture.root.path().join("codex.pid");
    executable(
        &fixture.codex,
        &format!(
            "if [ \"${{1:-}}\" = --version ]; then exit 0; fi\nprintf '%s\\n' \"$$\" >'{}'\nexec sleep 60",
            codex_pid.display()
        ),
    );
    compose_current(
        &fixture.state,
        "manifest_version: 1\ndefaults:\n  implementer_ceilings: {wall: 2s}\n",
    );
    let unit_arguments = fixture.root.path().join("unit-arguments");
    let systemd_run = fixture.root.path().join("systemd-run-records-and-executes");
    executable(
        &systemd_run,
        concat!(
            "printf '%s\\n' \"$@\" >\"$FAKE_UNIT_ARGUMENTS\"\n",
            "while [ \"$#\" -gt 0 ]; do\n",
            "  case \"$1\" in\n",
            "    --setenv) export \"$2\"; shift 2 ;;\n",
            "    --unit|--description|--property) shift 2 ;;\n",
            "    --*) shift ;;\n",
            "    *) break ;;\n",
            "  esac\n",
            "done\n",
            "\"$@\" >>\"$FAKE_IMPLEMENTER_LOG\" 2>&1 || true"
        ),
    );
    let implementer_log = fixture.root.path().join("implementer.log");

    let started = Instant::now();
    let output = fixture
        .dispatch(false)
        .env("MANDATE_GH_AS_BIN", &credential)
        .env("MANDATE_SYSTEMD_RUN_BIN", &systemd_run)
        .env("FAKE_UNIT_ARGUMENTS", &unit_arguments)
        .env("FAKE_IMPLEMENTER_LOG", &implementer_log)
        .output()
        .expect("dispatch an implementer that never finishes");
    let elapsed = started.elapsed();

    let log = fs::read_to_string(&implementer_log).unwrap_or_default();
    let trace = trace(&fixture.state);
    let dispatched = trace
        .iter()
        .find(|row| row["kind"] == "work-dispatched")
        .cloned()
        .unwrap_or(Value::Null);
    let order_id = dispatched["fact"]["order_id"].clone();
    let terminal = trace
        .iter()
        .find(|row| {
            matches!(row["kind"].as_str(), Some("work-completed" | "work-failed"))
                && row["fact"]["order_id"] == order_id
        })
        .cloned()
        .unwrap_or(Value::Null);
    let run_started = run_started_events(&fixture.state)
        .into_iter()
        .find(|event| event["payload"]["workOrder"] == order_id)
        .unwrap_or(Value::Null);
    let codex = fs::read_to_string(&codex_pid)
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok());
    let unit = fs::read_to_string(&unit_arguments).unwrap_or_default();
    assert_eq!(
        json!({
            "dispatched": output.status.success(),
            "work_dispatched_wall_seconds": dispatched["fact"]["wall_seconds"],
            "run_started_wall_ms": run_started["payload"]["ceilings"]["wallMs"],
            "terminal_kind": terminal["kind"],
            "terminal_reason": terminal["fact"]["reason"],
            "codex_started": codex.is_some(),
            "codex_still_running": codex.is_some_and(pid_alive),
            "stopped_long_before_codex_would_exit": elapsed < Duration::from_secs(30),
            "unit_outer_bound": unit.lines().any(|line| line == "RuntimeMaxSec=122"),
        }),
        json!({
            "dispatched": true,
            "work_dispatched_wall_seconds": 2,
            "run_started_wall_ms": 2000,
            "terminal_kind": "work-failed",
            "terminal_reason": "wall-cap",
            "codex_started": true,
            "codex_still_running": false,
            "stopped_long_before_codex_would_exit": true,
            "unit_outer_bound": true,
        }),
        "dispatch stderr: {}\nimplementer log: {log}\nunit arguments: {unit}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// #619 change 2: a hold whose process is alive but silent past its idle cap
/// is reaped by the next `ostrom dispatch` before that dispatch counts
/// in-flight holds. The process is gone, its lease is released, its
/// `work-failed` says `stalled` and charges the order's cost ceiling, and a
/// second dispatch of the same item proceeds instead of being refused.
///
/// #635 N1: only a dispatch scoped to the hold's repository reaps it. A hand
/// run with no repository scope reaps nothing and is refused by the live hold.
#[test]
fn a_live_silent_hold_is_reaped_by_dispatch_before_the_concurrency_count() {
    let fixture = DispatchFixture::new(false);
    compose_current(
        &fixture.state,
        "manifest_version: 1\ndefaults:\n  implementer_ceilings: {idle: 1s}\n",
    );
    let worker = fixture.root.path().join("silent-implementer-stub");
    executable(&worker, "exec sleep 60");
    let process_dispatch = || {
        fixture
            .dispatch(false)
            .env("OSTROM_EFFECTIVE_REPOSITORIES", "placeholder-org/alpha")
            .env("MANDATE_DISPATCH_BACKEND", "process")
            .env("MANDATE_OSTROM_BIN", &worker)
            .env("MANDATE_IMPLEMENTER_STARTUP_GRACE_MILLISECONDS", "100")
            .output()
            .expect("dispatch through the process backend")
    };
    let first_order: Value =
        serde_json::from_slice(&fs::read(&fixture.order_file).expect("read order"))
            .expect("order JSON");
    let first = process_dispatch();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_pid = process_lease(&fixture)["pid"].as_u64().expect("lease pid") as u32;
    let _first_guard = KillProcessGroup(first_pid);
    // Past the one-second idle cap at second precision.
    thread::sleep(Duration::from_secs(3));

    // A new order for the same item, as the next pass would write.
    let mut second_order = first_order.clone();
    second_order["order_id"] =
        json!("1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    fs::write(&fixture.order_file, format!("{second_order}\n")).expect("write second order");
    let unscoped = fixture
        .dispatch(false)
        .env_remove("OSTROM_EFFECTIVE_REPOSITORIES")
        .env("MANDATE_DISPATCH_BACKEND", "process")
        .env("MANDATE_OSTROM_BIN", &worker)
        .env("MANDATE_IMPLEMENTER_STARTUP_GRACE_MILLISECONDS", "100")
        .output()
        .expect("dispatch with no repository scope");
    let unscoped_left_it_running = pid_alive(first_pid);
    let second = process_dispatch();
    let second_pid = fs::read(
        fixture
            .state
            .join(format!("implementer-item-{}.lease", fixture.item_hash)),
    )
    .ok()
    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    .and_then(|lease| lease["pid"].as_u64())
    .map(|pid| pid as u32);
    let _second_guard = second_pid.map(KillProcessGroup);
    wait_until(Duration::from_secs(15), || !pid_alive(first_pid));

    let trace = trace(&fixture.state);
    let row = |kind: &str, order: &Value| {
        trace
            .iter()
            .find(|row| row["kind"] == kind && row["fact"]["order_id"] == *order)
            .cloned()
            .unwrap_or(Value::Null)
    };
    let first_dispatched = row("work-dispatched", &first_order["order_id"]);
    let reaped = row("work-failed", &first_order["order_id"]);
    assert_eq!(
        json!({
            "unscoped_dispatch_refused": !unscoped.status.success(),
            "unscoped_dispatch_left_it_running": unscoped_left_it_running,
            "second_dispatch_succeeded": second.status.success(),
            "first_process_running": pid_alive(first_pid),
            "reason": reaped["fact"]["reason"],
            "run_id_matches_the_hold": reaped["fact"]["run_id"] == first_dispatched["fact"]["run_id"],
            "cost_usd_is_the_ceiling": reaped["fact"]["cost_usd"].as_f64()
                == first_dispatched["fact"]["cost_ceiling_usd"].as_f64(),
            "cost_usd_is_a_number": reaped["fact"]["cost_usd"].is_number(),
            "names_its_last_progress": reaped["fact"]["last_progress_at"].is_string(),
            "stalled_past_the_cap": reaped["fact"]["stalled_seconds"]
                .as_u64()
                .is_some_and(|seconds| seconds > 1),
            "lease_names_a_new_process": second_pid.is_some_and(|pid| pid != first_pid),
            "second_order_dispatched": !row("work-dispatched", &second_order["order_id"]).is_null(),
        }),
        json!({
            "unscoped_dispatch_refused": true,
            "unscoped_dispatch_left_it_running": true,
            "second_dispatch_succeeded": true,
            "first_process_running": false,
            "reason": "stalled",
            "run_id_matches_the_hold": true,
            "cost_usd_is_the_ceiling": true,
            "cost_usd_is_a_number": true,
            "names_its_last_progress": true,
            "stalled_past_the_cap": true,
            "lease_names_a_new_process": true,
            "second_order_dispatched": true,
        }),
        "second dispatch stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
}

/// #619: the stall reaper writes an order's `work-failed` before it signals
/// the implementer. The implementer, stopping on that signal, must not add a
/// second terminal row for the same order.
#[test]
fn an_implementer_whose_order_the_reaper_already_closed_writes_no_second_terminal_row() {
    let fixture = DispatchFixture::new(false);
    let (_codex_environment, credential) = runnable_implementer(&fixture);
    let codex_pid = fixture.root.path().join("codex.pid");
    executable(
        &fixture.codex,
        &format!(
            "if [ \"${{1:-}}\" = --version ]; then exit 0; fi\nprintf '%s\\n' \"$$\" >'{}'\nexec sleep 60",
            codex_pid.display()
        ),
    );
    let unit = "ostrom-implementer-reaped";
    fs::write(
        fixture
            .state
            .join(format!("implementer-item-{}.lease", fixture.item_hash)),
        format!("{{\"owner\":\"{unit}\",\"started_at\":1,\"expires_at\":9999999999}}\n"),
    )
    .expect("write the implementer lease");
    let order_id = WorkOrder::from_json(&fs::read(&fixture.order_file).expect("read order"))
        .expect("valid work order")
        .order_id;
    let implementer = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .arg("implement")
        .arg(&fixture.order_file)
        .arg(unit)
        .current_dir(fixture.root.path())
        .env_clear()
        .env("OSTROM_HOME", &fixture.state)
        .env("HOME", &fixture.home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("OSTROM_PLUGIN_ROOT", plugin_root())
        .env("MANDATE_IMPLEMENTER_SOURCE_REPO", &fixture.source)
        .env("MANDATE_GH_AS_BIN", &credential)
        .env("CODEX_BIN", &fixture.codex)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the implementer");
    let implementer_pid = implementer.id();
    let mut implementer = implementer;
    wait_until(Duration::from_secs(30), || {
        fs::read_to_string(&codex_pid).is_ok_and(|pid| !pid.trim().is_empty())
    });
    let run_id = run_started_events(&fixture.state)
        .into_iter()
        .find(|event| event["payload"]["workOrder"] == order_id.as_str())
        .map(|event| event["runId"].clone())
        .expect("the implementer's run.started");
    let reaped = json!({
        "ts": "2026-08-01T00:00:00Z",
        "kind": "work-failed",
        "fact": {
            "schema_version": 1,
            "item_id": "placeholder-org/alpha#7",
            "order_id": order_id,
            "unit_name": unit,
            "run_id": run_id,
            "reason": "stalled",
            "cost_usd": 20.0,
            "reaped": true,
        },
        "narration": {},
    });
    let mut appended = fs::read_to_string(fixture.state.join("sprint.jsonl")).unwrap_or_default();
    appended.push_str(&format!("{reaped}\n"));
    fs::write(fixture.state.join("sprint.jsonl"), appended).expect("record the reaper's row");
    kill_pid(implementer_pid, "TERM");
    let _ = implementer.wait();

    let terminal = trace(&fixture.state)
        .into_iter()
        .filter(|row| {
            matches!(row["kind"].as_str(), Some("work-completed" | "work-failed"))
                && row["fact"]["order_id"] == order_id.as_str()
        })
        .collect::<Vec<_>>();
    assert_eq!(terminal.len(), 1, "{terminal:?}");
    assert_eq!(terminal[0]["fact"]["reason"], "stalled");
}

/// #635: a stalled implementer that handles `SIGTERM` writes its own terminal
/// row when `ostrom up` stops it. The reaper's claim on its run makes that row
/// say `stalled` and charge the order's cost ceiling, so there is exactly one
/// row, written by the run, saying why it ended.
#[test]
fn a_term_handling_implementer_stopped_by_the_reaper_writes_one_stalled_row() {
    use std::os::unix::process::CommandExt as _;

    let fixture = DispatchFixture::new(false);
    compose_current(&fixture.state, "manifest_version: 1\n");
    let (_codex_environment, credential) = runnable_implementer(&fixture);
    let codex_pid = fixture.root.path().join("codex.pid");
    executable(
        &fixture.codex,
        &format!(
            "if [ \"${{1:-}}\" = --version ]; then exit 0; fi\nprintf '%s\\n' \"$$\" >'{}'\nexec sleep 60",
            codex_pid.display()
        ),
    );
    let unit = "ostrom-implementer-stalled";
    let run_id = "implementer-stalled-placeholder-run";
    let lease = fixture
        .state
        .join(format!("implementer-item-{}.lease", fixture.item_hash));
    fs::write(
        &lease,
        format!("{{\"owner\":\"{unit}\",\"started_at\":1,\"expires_at\":9999999999}}\n"),
    )
    .expect("write the implementer lease");
    let order = WorkOrder::from_json(&fs::read(&fixture.order_file).expect("read order"))
        .expect("valid work order");
    // The hold, as dispatch records one on the process backend, with a
    // one-second idle cap.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is past the epoch")
        .as_secs();
    let dispatched_at = chrono::DateTime::<chrono::Utc>::from_timestamp(
        i64::try_from(now - 60).expect("a timestamp in range"),
        0,
    )
    .expect("valid timestamp")
    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut appended = fs::read_to_string(fixture.state.join("sprint.jsonl")).unwrap_or_default();
    appended.push_str(&format!(
        "{}\n",
        json!({
            "ts": dispatched_at,
            "kind": "work-dispatched",
            "fact": {
                "schema_version": 1,
                "item_id": &order.item_id,
                "order_id": &order.order_id,
                "unit_name": unit,
                "backend": "process",
                "run_id": run_id,
                "runner": "agent/codex",
                "wall_seconds": 14_400,
                "idle_seconds": 1,
                "cost_ceiling_usd": 20,
                "token_ceiling": 500_000,
                "cost_usd": null,
                "duration_seconds": 0,
            },
            "narration": {},
        })
    ));
    fs::write(fixture.state.join("sprint.jsonl"), appended).expect("record the dispatch");
    let mut implementer = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .arg("implement")
        .arg(&fixture.order_file)
        .arg(unit)
        .args(["--run-id", run_id])
        .current_dir(fixture.root.path())
        .env_clear()
        .env("OSTROM_HOME", &fixture.state)
        .env("HOME", &fixture.home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("OSTROM_PLUGIN_ROOT", plugin_root())
        .env("MANDATE_IMPLEMENTER_SOURCE_REPO", &fixture.source)
        .env("MANDATE_GH_AS_BIN", &credential)
        .env("MANDATE_IMPLEMENTER_TERMINATION_GRACE_SECONDS", "1")
        .env("CODEX_BIN", &fixture.codex)
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the implementer");
    let implementer_pid = implementer.id();
    let _group = KillProcessGroup(implementer_pid);
    wait_until(Duration::from_secs(30), || {
        fs::read_to_string(&codex_pid).is_ok_and(|pid| !pid.trim().is_empty())
    });
    // Bound to its process, as the process backend binds a dispatched one.
    let (group, _, start_time) =
        proc_identity(implementer_pid).expect("the implementer's process identity");
    fs::write(
        &lease,
        json!({
            "owner": unit,
            "started_at": 1,
            "expires_at": 9_999_999_999_u64,
            "pid": implementer_pid,
            "process_group_id": group,
            "process_start_time": start_time,
        })
        .to_string(),
    )
    .expect("bind the lease to the implementer");
    // Past the one-second idle cap at second precision.
    thread::sleep(Duration::from_secs(3));

    let up = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .arg("up")
        .current_dir(&fixture.state)
        .env("OSTROM_HOME", &fixture.state)
        .env(
            "OSTROM_POLICY_TRUSTED_KEYS",
            fixture.state.join("trusted-policy-keys"),
        )
        .env_remove("OSTROM_POLICY_MANIFEST")
        .env_remove("OSTROM_RUN_ID")
        .env_remove("CLAUDE_CONFIG_DIR")
        .output()
        .expect("run ostrom up");
    let deadline = Instant::now() + Duration::from_secs(30);
    while implementer
        .try_wait()
        .expect("poll the implementer")
        .is_none()
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(50));
    }

    let terminal = trace(&fixture.state)
        .into_iter()
        .filter(|row| {
            matches!(row["kind"].as_str(), Some("work-completed" | "work-failed"))
                && row["fact"]["order_id"] == order.order_id.as_str()
        })
        .collect::<Vec<_>>();
    let row = terminal.first().cloned().unwrap_or(Value::Null);
    assert_eq!(
        json!({
            "up_succeeded": up.status.success(),
            "up_reaped_it": String::from_utf8_lossy(&up.stdout).contains("reaped=1"),
            "terminal_rows": terminal.len(),
            "reason": row["fact"]["reason"],
            "cost_usd": row["fact"]["cost_usd"].as_f64(),
            "cost_basis": row["fact"]["cost_basis"],
            "reaped": row["fact"]["reaped"],
            "written_by_the_run": row["fact"]
                .as_object()
                .is_some_and(|fact| fact.contains_key("worktree_path")),
            "claim_removed": !fixture
                .state
                .join(format!("reaping/{run_id}.claim"))
                .exists(),
            "lease_released": !lease.exists(),
        }),
        json!({
            "up_succeeded": true,
            "up_reaped_it": true,
            "terminal_rows": 1,
            "reason": "stalled",
            "cost_usd": 20.0,
            "cost_basis": "declared-ceiling",
            "reaped": true,
            "written_by_the_run": true,
            "claim_removed": true,
            "lease_released": true,
        }),
        "up stderr: {}",
        String::from_utf8_lossy(&up.stderr)
    );
}

/// #633: an implementer whose worker hangs while its harness ignores `SIGTERM`
/// is stopped whole. Codex leads a process group of its own, which no signal
/// to the implementer's group reaches: without its recorded identity the
/// reaper's `KILL` would remove the supervisor and the worker and leave Codex
/// running in the item worktree after the lease is released.
#[test]
fn a_reaped_implementer_whose_harness_ignores_term_leaves_no_orphan() {
    use std::os::unix::process::CommandExt as _;

    let fixture = DispatchFixture::new(false);
    compose_current(&fixture.state, "manifest_version: 1\n");
    let (_codex_environment, credential) = runnable_implementer(&fixture);
    let codex_pid = fixture.root.path().join("codex.pid");
    // Codex ignores TERM, then freezes the worker that started it, standing
    // for a worker that never reaches its own stop of the harness.
    executable(
        &fixture.codex,
        &format!(
            concat!(
                "if [ \"${{1:-}}\" = --version ]; then exit 0; fi\n",
                "trap '' TERM\n",
                "printf '%s\\n' \"$$\" >'{}'\n",
                "sleep 2\n",
                "kill -STOP \"$PPID\"\n",
                "exec sleep 120"
            ),
            codex_pid.display()
        ),
    );
    let unit = "ostrom-implementer-orphan";
    let run_id = "implementer-orphan-placeholder-run";
    let lease = fixture
        .state
        .join(format!("implementer-item-{}.lease", fixture.item_hash));
    fs::write(
        &lease,
        format!("{{\"owner\":\"{unit}\",\"started_at\":1,\"expires_at\":9999999999}}\n"),
    )
    .expect("write the implementer lease");
    let order = WorkOrder::from_json(&fs::read(&fixture.order_file).expect("read order"))
        .expect("valid work order");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is past the epoch")
        .as_secs();
    let dispatched_at = chrono::DateTime::<chrono::Utc>::from_timestamp(
        i64::try_from(now - 60).expect("a timestamp in range"),
        0,
    )
    .expect("valid timestamp")
    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut appended = fs::read_to_string(fixture.state.join("sprint.jsonl")).unwrap_or_default();
    appended.push_str(&format!(
        "{}\n",
        json!({
            "ts": dispatched_at,
            "kind": "work-dispatched",
            "fact": {
                "schema_version": 1,
                "item_id": &order.item_id,
                "order_id": &order.order_id,
                "unit_name": unit,
                "backend": "process",
                "run_id": run_id,
                "runner": "agent/codex",
                "wall_seconds": 14_400,
                "idle_seconds": 1,
                "cost_ceiling_usd": 20,
                "token_ceiling": 500_000,
                "cost_usd": null,
                "duration_seconds": 0,
            },
            "narration": {},
        })
    ));
    fs::write(fixture.state.join("sprint.jsonl"), appended).expect("record the dispatch");
    let mut implementer = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .arg("implement")
        .arg(&fixture.order_file)
        .arg(unit)
        .args(["--run-id", run_id])
        .current_dir(fixture.root.path())
        .env_clear()
        .env("OSTROM_HOME", &fixture.state)
        .env("HOME", &fixture.home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("OSTROM_PLUGIN_ROOT", plugin_root())
        .env("MANDATE_IMPLEMENTER_SOURCE_REPO", &fixture.source)
        .env("MANDATE_GH_AS_BIN", &credential)
        .env("MANDATE_IMPLEMENTER_TERMINATION_GRACE_SECONDS", "1")
        .env("CODEX_BIN", &fixture.codex)
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the implementer");
    let implementer_pid = implementer.id();
    let _group = KillProcessGroup(implementer_pid);
    wait_until(Duration::from_secs(30), || {
        fs::read_to_string(&codex_pid).is_ok_and(|pid| !pid.trim().is_empty())
    });
    let codex = fs::read_to_string(&codex_pid)
        .expect("read the Codex pid")
        .trim()
        .parse::<u32>()
        .expect("a Codex pid");
    let codex_start_time = support::process_start_time(codex).expect("Codex's start time");
    let _codex = support::KillSameProcessGroup(codex, codex_start_time);
    let (group, _, start_time) =
        proc_identity(implementer_pid).expect("the implementer's process identity");
    fs::write(
        &lease,
        json!({
            "owner": unit,
            "started_at": 1,
            "expires_at": 9_999_999_999_u64,
            "pid": implementer_pid,
            "process_group_id": group,
            "process_start_time": start_time,
        })
        .to_string(),
    )
    .expect("bind the lease to the implementer");
    // Past the idle cap, and past the moment Codex freezes the worker.
    thread::sleep(Duration::from_secs(4));

    let up = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .arg("up")
        .current_dir(&fixture.state)
        .env("OSTROM_HOME", &fixture.state)
        .env(
            "OSTROM_POLICY_TRUSTED_KEYS",
            fixture.state.join("trusted-policy-keys"),
        )
        .env_remove("OSTROM_POLICY_MANIFEST")
        .env_remove("OSTROM_RUN_ID")
        .env_remove("CLAUDE_CONFIG_DIR")
        .output()
        .expect("run ostrom up");
    let deadline = Instant::now() + Duration::from_secs(60);
    while (implementer
        .try_wait()
        .expect("poll the implementer")
        .is_none()
        || support::same_process_running(codex, codex_start_time))
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(50));
    }

    let terminal = trace(&fixture.state)
        .into_iter()
        .filter(|row| {
            matches!(row["kind"].as_str(), Some("work-completed" | "work-failed"))
                && row["fact"]["order_id"] == order.order_id.as_str()
        })
        .collect::<Vec<_>>();
    let row = terminal.first().cloned().unwrap_or(Value::Null);
    assert_eq!(
        json!({
            "up_reaped_it": String::from_utf8_lossy(&up.stdout).contains("reaped=1"),
            "implementer_stopped": implementer.try_wait().expect("poll the implementer").is_some(),
            "codex_running": support::same_process_running(codex, codex_start_time),
            "terminal_rows": terminal.len(),
            "reason": row["fact"]["reason"],
            "claim_removed": !fixture
                .state
                .join(format!("reaping/{run_id}.claim"))
                .exists(),
            "lease_released": !lease.exists(),
            "harness_records_left": support::harness_records(&fixture.state),
        }),
        json!({
            "up_reaped_it": true,
            "implementer_stopped": true,
            "codex_running": false,
            "terminal_rows": 1,
            "reason": "stalled",
            "claim_removed": true,
            "lease_released": true,
            "harness_records_left": 0,
        }),
        "up stderr: {}",
        String::from_utf8_lossy(&up.stderr)
    );
}

const TWO_RUNNERS: &str = concat!(
    "manifest_version: 1\n",
    "defaults:\n",
    "  runner_retry: 2h\n",
    "  implementers:\n",
    "    - {runner: agent/codex}\n",
    "    - {runner: agent/claude, model: claude-placeholder}\n",
);

/// A `systemd-run` stub that runs the implementer to completion in place.
fn executing_systemd_run(fixture: &DispatchFixture) -> PathBuf {
    let systemd_run = fixture.root.path().join("systemd-run-executes");
    executable(
        &systemd_run,
        concat!(
            "while [ \"$#\" -gt 0 ]; do\n",
            "  case \"$1\" in\n",
            "    --setenv) export \"$2\"; shift 2 ;;\n",
            "    --unit|--description|--property) shift 2 ;;\n",
            "    --*) shift ;;\n",
            "    *) break ;;\n",
            "  esac\n",
            "done\n",
            "\"$@\" >>\"$FAKE_IMPLEMENTER_LOG\" 2>&1 || true"
        ),
    );
    systemd_run
}

fn write_availability(state: &Path, runners: &[(&str, &str)]) {
    let runners = runners
        .iter()
        .map(|(runner, until)| {
            (
                (*runner).to_owned(),
                json!({
                    "until": until,
                    "reset_reported": true,
                    "reason": "usage-limit",
                    "message": "placeholder limit",
                    "recorded_at": "2026-09-28T00:00:00Z",
                    "run_id": "placeholder-run",
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    fs::write(
        state.join("runner-availability.json"),
        json!({"schema_version": 1, "runners": runners}).to_string(),
    )
    .expect("write runner availability");
}

/// #626: a runner that refuses on an allowance limit ends its run with
/// `runner-unavailable` and is marked unavailable; the next dispatch of the
/// same item goes to the next declared runner and records `work-rerouted`.
/// The refusal says nothing about the item, so two of them never escalate.
#[test]
fn a_usage_limit_reroutes_the_next_dispatch_and_never_escalates() {
    let fixture = DispatchFixture::new(false);
    let (_codex_environment, credential) = runnable_implementer(&fixture);
    executable(
        &fixture.codex,
        r#"if [ "${1:-}" = --version ]; then exit 0; fi
echo "{\"type\":\"error\",\"message\":\"You've hit your usage limit. Try again later.\"}"
exit 1"#,
    );
    compose_current(&fixture.state, TWO_RUNNERS);
    let implementer_log = fixture.root.path().join("implementer.log");
    let first = fixture
        .dispatch(false)
        .env("MANDATE_GH_AS_BIN", &credential)
        .env("MANDATE_SYSTEMD_RUN_BIN", executing_systemd_run(&fixture))
        .env("FAKE_IMPLEMENTER_LOG", &implementer_log)
        .output()
        .expect("dispatch to a Codex out of allowance");
    // A second refusal of the same item: two identical failures would
    // otherwise escalate and refuse the next dispatch.
    let mut trace_file = fs::OpenOptions::new()
        .append(true)
        .open(fixture.state.join("sprint.jsonl"))
        .expect("open trace");
    std::io::Write::write_all(
        &mut trace_file,
        format!(
            "{}\n",
            json!({
                "ts": "2026-09-28T00:00:00Z",
                "kind": "work-failed",
                "fact": {
                    "schema_version": 1,
                    "item_id": "placeholder-org/alpha#7",
                    "order_id": "2123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    "reason": "runner-unavailable",
                    "runner": "agent/codex",
                },
                "narration": {},
            })
        )
        .as_bytes(),
    )
    .expect("append a second refusal");
    let (claude, _canary_calls) = claude_canary_stub(&fixture, "DENIED");
    let (path, _) = control_curl(&fixture, 0);
    let second = fixture
        .dispatch(false)
        .env("PATH", path)
        .env("MANDATE_GH_AS_BIN", &credential)
        .env("CLAUDE_BIN", &claude)
        .output()
        .expect("dispatch the same item again");

    let trace = trace(&fixture.state);
    let refused = trace
        .iter()
        .find(|row| row["kind"] == "work-failed")
        .cloned()
        .unwrap_or(Value::Null);
    let rerouted = trace
        .iter()
        .find(|row| row["kind"] == "work-rerouted")
        .cloned()
        .unwrap_or(Value::Null);
    let last_dispatched = trace
        .iter()
        .rev()
        .find(|row| row["kind"] == "work-dispatched")
        .cloned()
        .unwrap_or(Value::Null);
    let availability: Value = fs::read(fixture.state.join("runner-availability.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    let unit = fs::read_to_string(&fixture.systemd_args).unwrap_or_default();
    assert_eq!(
        json!({
            "first_dispatched": first.status.success(),
            "refusal_reason": refused["fact"]["reason"],
            "refusal_runner": refused["fact"]["runner"],
            "codex_reset_reported": availability["runners"]["agent/codex"]["reset_reported"],
            "second_dispatched": second.status.success(),
            "rerouted_from": rerouted["fact"]["from"],
            "rerouted_to": rerouted["fact"]["to"],
            "rerouted_until_recorded": rerouted["fact"]["until"].is_string(),
            "rerouted_item": rerouted["fact"]["item_id"],
            "dispatched_runner": last_dispatched["fact"]["runner"],
            "unit_runs_claude": unit.lines().any(|line| line == "agent/claude"),
            "unit_passes_model": unit.lines().any(|line| line == "--model=claude-placeholder"),
            "escalated": trace.iter().any(|row| row["kind"] == "dispatch-failure-escalated"),
        }),
        json!({
            "first_dispatched": true,
            "refusal_reason": "runner-unavailable",
            "refusal_runner": "agent/codex",
            "codex_reset_reported": false,
            "second_dispatched": true,
            "rerouted_from": "agent/codex",
            "rerouted_to": "agent/claude",
            "rerouted_until_recorded": true,
            "rerouted_item": "placeholder-org/alpha#7",
            "dispatched_runner": "agent/claude",
            "unit_runs_claude": true,
            "unit_passes_model": true,
            "escalated": false,
        }),
        "first: {}\nsecond: {}\nimplementer log: {}",
        String::from_utf8_lossy(&first.stderr),
        String::from_utf8_lossy(&second.stderr),
        fs::read_to_string(&implementer_log).unwrap_or_default()
    );
}

/// #626: when every declared runner is unavailable, dispatch holds the item
/// with a `decision.requested` naming the earliest reset and exits non-zero.
#[test]
fn every_runner_unavailable_is_a_decision_and_a_non_zero_exit() {
    let fixture = DispatchFixture::new(false);
    compose_current(&fixture.state, TWO_RUNNERS);
    write_availability(
        &fixture.state,
        &[
            ("agent/codex", "2099-01-02T00:00:00Z"),
            ("agent/claude", "2099-01-01T00:00:00Z"),
        ],
    );
    let output = fixture.dispatch(false).output().expect("dispatch");
    let trace = trace(&fixture.state);
    let decision = trace
        .iter()
        .find(|row| row["kind"] == "decision-requested")
        .cloned()
        .unwrap_or(Value::Null);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        json!({
            "exit": output.status.code(),
            "decision_subject": decision["fact"]["subject"],
            "decision_kind": decision["fact"]["kind"],
            "names_earliest_reset": stderr.contains("agent/claude until 2099-01-01T00:00:00Z"),
            "dispatched": trace.iter().any(|row| row["kind"] == "work-dispatched"),
        }),
        json!({
            "exit": 3,
            "decision_subject": "placeholder-org/alpha#7",
            "decision_kind": "human_decides",
            "names_earliest_reset": true,
            "dispatched": false,
        }),
        "{stderr}"
    );
}

/// #626: a runner whose recorded reset has passed is available again, and
/// with no policy the order is `[agent/codex]`.
#[test]
fn a_reset_in_the_past_makes_the_runner_available_again() {
    let fixture = DispatchFixture::new(false);
    write_availability(&fixture.state, &[("agent/codex", "2000-01-01T00:00:00Z")]);
    let output = fixture.dispatch(false).output().expect("dispatch");
    let trace = trace(&fixture.state);
    let dispatched = trace
        .iter()
        .find(|row| row["kind"] == "work-dispatched")
        .cloned()
        .unwrap_or(Value::Null);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(dispatched["fact"]["runner"], "agent/codex");
    assert!(!trace.iter().any(|row| row["kind"] == "work-rerouted"));
}

/// #626: `--runner` overrides the declared order for a hand run.
#[test]
fn the_runner_flag_overrides_the_declared_order() {
    let fixture = DispatchFixture::new(false);
    compose_current(&fixture.state, TWO_RUNNERS);
    let (claude, _canary_calls) = claude_canary_stub(&fixture, "DENIED");
    let (path, _) = control_curl(&fixture, 0);
    let output = fixture
        .dispatch(false)
        .env("PATH", path)
        .arg("--runner")
        .arg("agent/claude")
        .env("CLAUDE_BIN", &claude)
        .output()
        .expect("dispatch with --runner");
    let trace = trace(&fixture.state);
    let dispatched = trace
        .iter()
        .find(|row| row["kind"] == "work-dispatched")
        .cloned()
        .unwrap_or(Value::Null);
    let unit = fs::read_to_string(&fixture.systemd_args).unwrap_or_default();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(dispatched["fact"]["runner"], "agent/claude");
    assert!(unit.lines().any(|line| line == "agent/claude"), "{unit}");
    assert!(!trace.iter().any(|row| row["kind"] == "work-rerouted"));
}

/// A Claude stub for the sandbox canary: it reports the version in
/// `claude.version` (2.1.283 when absent), counts each session, runs "the
/// command" in its working directory and reports the network as `network`.
fn claude_canary_stub(fixture: &DispatchFixture, network: &str) -> (PathBuf, PathBuf) {
    let claude = fixture.root.path().join("claude-stub");
    let calls = fixture.root.path().join("claude.calls");
    let version = fixture.root.path().join("claude.version");
    executable(
        &claude,
        &format!(
            concat!(
                "if [ \"${{1:-}}\" = --version ]; then echo \"$(cat '{version}' 2>/dev/null || echo 2.1.283) (Claude Code)\"; exit 0; fi\n",
                "printf '%s\\n' call >>'{calls}'\n",
                "cat >/dev/null\n",
                "touch inside.marker\n",
                "echo '{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"t1\",\"content\":\"CANARY-NETWORK-{network}\"}}]}}}}'\n",
                "echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"DONE\",\"total_cost_usd\":0.01}}'",
            ),
            version = version.display(),
            calls = calls.display(),
            network = network,
        ),
    );
    (claude, calls)
}

/// A `curl` for the canary's unsandboxed control that exits `exit` and records
/// the proxy variables it saw. Returns a `PATH` that finds it first.
fn control_curl(fixture: &DispatchFixture, exit: i32) -> (String, PathBuf) {
    let directory = fixture.root.path().join("control-bin");
    fs::create_dir_all(&directory).expect("create control bin");
    let seen = fixture.root.path().join("control.env");
    executable(
        &directory.join("curl"),
        &format!(
            "env | grep -i '_proxy=' >'{}' || true\nexit {exit}",
            seen.display()
        ),
    );
    let path = format!(
        "{}:{}",
        directory.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (path, seen)
}

fn canary_calls(calls: &Path) -> usize {
    fs::read_to_string(calls).map_or(0, |calls| calls.lines().count())
}

/// #626: Claude Code runs without a settings file it cannot read, so Claude is
/// used only after a canary saw its sandbox deny the network. A Claude whose
/// canary reaches the network is marked unavailable with `sandbox-unverified`
/// and routing moves on to the next runner.
#[test]
fn a_claude_whose_canary_reaches_the_network_is_skipped_for_the_next_runner() {
    let fixture = DispatchFixture::new(false);
    compose_current(
        &fixture.state,
        "manifest_version: 1\ndefaults:\n  implementers: [{runner: agent/claude}, {runner: agent/codex}]\n",
    );
    let (claude, calls) = claude_canary_stub(&fixture, "REACHED");
    let (path, _) = control_curl(&fixture, 0);
    let output = fixture
        .dispatch(false)
        .env("PATH", path)
        .env("CLAUDE_BIN", &claude)
        .output()
        .expect("dispatch");
    let trace = trace(&fixture.state);
    let row = |kind: &str| {
        trace
            .iter()
            .find(|row| row["kind"] == kind)
            .cloned()
            .unwrap_or(Value::Null)
    };
    let availability: Value = fs::read(fixture.state.join("runner-availability.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    assert_eq!(
        json!({
            "dispatched": output.status.success(),
            "canary_ran": canary_calls(&calls),
            "canary_outcome": row("sandbox-checked")["fact"]["outcome"],
            "canary_cost_recorded": row("sandbox-checked")["fact"]["cost_usd"],
            "claude_reason": availability["runners"]["agent/claude"]["reason"],
            "rerouted_from": row("work-rerouted")["fact"]["from"],
            "runner": row("work-dispatched")["fact"]["runner"],
            "pass_cached": fixture.state.join("sandbox-canary.json").exists(),
        }),
        json!({
            "dispatched": true,
            "canary_ran": 1,
            "canary_outcome": "fail",
            "canary_cost_recorded": 0.01,
            "claude_reason": "sandbox-unverified",
            "rerouted_from": "agent/claude",
            "runner": "agent/codex",
            "pass_cached": false,
        }),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// #626: a denied sandboxed attempt means nothing unless ostrom itself can
/// reach the host. An unreachable control is inconclusive: no Claude session
/// is spent, nothing is cached, and routing moves on.
#[test]
fn an_unreachable_control_is_inconclusive_and_caches_nothing() {
    let fixture = DispatchFixture::new(false);
    compose_current(
        &fixture.state,
        "manifest_version: 1\ndefaults:\n  implementers: [{runner: agent/claude}, {runner: agent/codex}]\n",
    );
    let (claude, calls) = claude_canary_stub(&fixture, "DENIED");
    let (path, _) = control_curl(&fixture, 7);
    let output = fixture
        .dispatch(false)
        .env("PATH", path)
        .env("CLAUDE_BIN", &claude)
        .output()
        .expect("dispatch");
    let trace = trace(&fixture.state);
    let row = |kind: &str| {
        trace
            .iter()
            .find(|row| row["kind"] == kind)
            .cloned()
            .unwrap_or(Value::Null)
    };
    let availability: Value = fs::read(fixture.state.join("runner-availability.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    assert_eq!(
        json!({
            "dispatched": output.status.success(),
            "claude_sessions": canary_calls(&calls),
            "canary_outcome": row("sandbox-checked")["fact"]["outcome"],
            "claude_reason": availability["runners"]["agent/claude"]["reason"],
            "runner": row("work-dispatched")["fact"]["runner"],
            "pass_cached": fixture.state.join("sandbox-canary.json").exists(),
        }),
        json!({
            "dispatched": true,
            "claude_sessions": 0,
            "canary_outcome": "inconclusive",
            "claude_reason": "sandbox-inconclusive",
            "runner": "agent/codex",
            "pass_cached": false,
        }),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// #626: the control runs with the operator's proxy environment unchanged,
/// and the `sandbox-checked` fact records those variables with any
/// credential removed.
#[test]
fn the_control_sees_the_proxy_environment_and_the_record_never_holds_its_secret() {
    let fixture = DispatchFixture::new(false);
    let (claude, _calls) = claude_canary_stub(&fixture, "DENIED");
    let (path, seen) = control_curl(&fixture, 0);
    let output = fixture
        .dispatch(false)
        .env("PATH", path)
        .env("HTTPS_PROXY", "http://user:s3cret@proxy.invalid:3128")
        .env("no_proxy", "localhost,.internal.invalid")
        .arg("--runner=agent/claude")
        .env("CLAUDE_BIN", &claude)
        .output()
        .expect("dispatch");
    let control = fs::read_to_string(&seen).unwrap_or_default();
    let checked = trace(&fixture.state)
        .into_iter()
        .find(|row| row["kind"] == "sandbox-checked")
        .unwrap_or(Value::Null);
    let leaked = ["sprint.jsonl", "events.jsonl"].iter().any(|file| {
        fs::read_to_string(fixture.state.join(file)).is_ok_and(|text| text.contains("s3cret"))
    });
    assert_eq!(
        json!({
            "dispatched": output.status.success(),
            "control_saw_proxy": control
                .lines()
                .any(|line| line == "HTTPS_PROXY=http://user:s3cret@proxy.invalid:3128"),
            "control_saw_no_proxy": control
                .lines()
                .any(|line| line == "no_proxy=localhost,.internal.invalid"),
            "outcome": checked["fact"]["outcome"],
            "recorded_proxy": checked["fact"]["proxy"],
            "secret_in_a_record": leaked,
        }),
        json!({
            "dispatched": true,
            "control_saw_proxy": true,
            "control_saw_no_proxy": true,
            "outcome": "pass",
            "recorded_proxy": {
                "HTTPS_PROXY": "http://proxy.invalid:3128",
                "no_proxy": "localhost,.internal.invalid",
            },
            "secret_in_a_record": false,
        }),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// #626: a denied canary is cached per (claude version, profile hash); the
/// same binary is not checked again, and a new version is.
#[test]
fn a_passing_canary_is_cached_until_the_claude_version_changes() {
    let fixture = DispatchFixture::new(false);
    let (claude, calls) = claude_canary_stub(&fixture, "DENIED");
    let (path, _) = control_curl(&fixture, 0);
    let dispatch = || {
        fixture
            .dispatch(false)
            .env("PATH", &path)
            .arg("--runner=agent/claude")
            .env("CLAUDE_BIN", &claude)
            .output()
            .expect("dispatch")
    };
    let first = dispatch();
    let cache: Value = fs::read(fixture.state.join("sandbox-canary.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    let after_first = canary_calls(&calls);
    // The item is now held, so these dispatches are refused after the runner
    // is chosen; only whether the canary ran again matters here.
    let _ = dispatch();
    let after_same_version = canary_calls(&calls);
    fs::write(fixture.root.path().join("claude.version"), "2.1.300").expect("upgrade claude");
    let _ = dispatch();
    let after_upgrade = canary_calls(&calls);
    let dispatched = trace(&fixture.state)
        .into_iter()
        .find(|row| row["kind"] == "work-dispatched")
        .unwrap_or(Value::Null);
    assert_eq!(
        json!({
            "first_dispatched": first.status.success(),
            "runner": dispatched["fact"]["runner"],
            "cached_version": cache["runners"]["agent/claude"]["version"],
            "cached_hash_is_sha256": cache["runners"]["agent/claude"]["profile_sha256"]
                .as_str()
                .is_some_and(|hash| hash.len() == 64),
            "after_first": after_first,
            "after_same_version": after_same_version,
            "after_upgrade": after_upgrade,
        }),
        json!({
            "first_dispatched": true,
            "runner": "agent/claude",
            "cached_version": "2.1.283 (Claude Code)",
            "cached_hash_is_sha256": true,
            "after_first": 1,
            "after_same_version": 1,
            "after_upgrade": 2,
        }),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
}

/// Compose `manifest` as this state root's current policy version, signed as
/// the operator's, the way `ostrom compose` installs one.
fn compose_current(state: &Path, manifest: &str) {
    let path = state.join("ostrom.yaml");
    fs::write(&path, manifest).expect("write operator manifest");
    let trusted_keys = support::sign_manifest(&path);
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .current_dir(state)
        .env("OSTROM_HOME", state)
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted_keys)
        .env_remove("OSTROM_POLICY_MANIFEST")
        .arg("compose")
        .arg(&path)
        .output()
        .expect("compose the current policy version");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Make the fixture's implementer able to reach its harness: a source clone to
/// branch from, a credential stub that answers both dispatch's reads and the
/// implementer's, and a Codex stub that records its environment and fails.
/// Returns the recorded-environment path and the credential stub.
fn runnable_implementer(fixture: &DispatchFixture) -> (PathBuf, PathBuf) {
    let source = &fixture.source;
    git(source, &["init", "-b", "main"]);
    git(source, &["config", "user.email", "fixture@example.invalid"]);
    git(source, &["config", "user.name", "Fixture"]);
    fs::write(source.join("README.md"), "placeholder\n").expect("write source");
    git(source, &["add", "README.md"]);
    git(source, &["commit", "-m", "base"]);
    git(
        source,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/placeholder-org/alpha.git",
        ],
    );
    git(source, &["update-ref", "refs/remotes/origin/main", "HEAD"]);

    let codex_environment = fixture.root.path().join("codex.env");
    executable(
        &fixture.codex,
        &format!(
            "if [ \"${{1:-}}\" = --version ]; then exit 0; fi\nenv >'{}'\nexit 1",
            codex_environment.display()
        ),
    );
    // Answers both dispatch's reads and the implementer's, keyed on the
    // command after the wrapper's `--`.
    let credential = fixture.root.path().join("run-through-credential-stub");
    executable(
        &credential,
        concat!(
            "while [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n",
            "shift\n",
            "case \"$*\" in\n",
            "  *'/branches?'*) printf '%s\\n' '[{\"name\":\"main\",\"commit\":{\"sha\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}}]' ;;\n",
            "  *' issue view '*) printf '%s\\n' '{\"closedByPullRequestsReferences\":[]}' ;;\n",
            "  'gh pr list '*) printf '%s\\n' '[]' ;;\n",
            "  'gh repo view '*) printf '%s\\n' main ;;\n",
            "  'git -C '*' fetch '*) git -C \"$3\" update-ref refs/remotes/origin/main refs/heads/main ;;\n",
            "  *) exit 1 ;;\n",
            "esac"
        ),
    );
    (codex_environment, credential)
}

fn ps_json(state: &Path) -> Vec<Value> {
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["ps", "--json"])
        .env("OSTROM_HOME", state)
        .env_remove("CLAUDE_CONFIG_DIR")
        .output()
        .expect("run ps --json");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("ps --json is UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("ps --json line is JSON"))
        .collect()
}

fn run_started_events(state: &Path) -> Vec<Value> {
    let Ok(runs) = fs::read_dir(state.join("runs")) else {
        return Vec::new();
    };
    runs.flatten()
        .filter_map(|run| fs::read_to_string(run.path().join("events.jsonl")).ok())
        .filter_map(|events| {
            events
                .lines()
                .next()
                .and_then(|line| serde_json::from_str::<Value>(line).ok())
        })
        .filter(|event| event["type"] == "run.started")
        .collect()
}

fn git(path: &Path, arguments: &[&str]) {
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(arguments)
            .status()
            .expect("run git")
            .success(),
        "git {arguments:?}"
    );
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
