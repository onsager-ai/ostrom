#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::{Value, json};
use tempfile::TempDir;

const ITEM_ID: &str = "placeholder-org/alpha#42";
const DISPATCH_TIME: &str = "2026-08-01T00:00:00Z";
const DISPATCH_EPOCH: &str = "1785542400";
const STALE_EPOCH: &str = "1785548000";
const UNIT: &str = "ostrom-implementer-placeholder";

struct Fixture {
    root: TempDir,
    state: PathBuf,
    candidate: PathBuf,
    order: PathBuf,
    systemctl: PathBuf,
}

impl Fixture {
    fn new(systemctl_source: &str) -> Self {
        let root = tempfile::tempdir().expect("work-order fixture");
        let state = root.path().join("state");
        let candidate = root.path().join("candidate.json");
        fs::create_dir_all(&state).expect("create state");
        fs::write(
            &candidate,
            format!(
                "{}\n",
                json!({
                    "schema_version": 1,
                    "item_id": ITEM_ID,
                    "repository": "placeholder-org/alpha",
                    "item_ref": "#42",
                    "branch_name": "placeholder/overwritten",
                    "spec": "Change the placeholder fixture.",
                    "acceptance_criteria": ["The placeholder changes."],
                    "constraints": ["Use placeholder data only."]
                })
            ),
        )
        .expect("write candidate");
        let systemctl = root.path().join("systemctl-stub");
        executable(&systemctl, systemctl_source);
        let create = command(&root, &state, &systemctl)
            .env("MANDATE_TRACE_TIME", DISPATCH_TIME)
            .env("MANDATE_NOW_EPOCH", DISPATCH_EPOCH)
            .args(["work-order", "create", candidate.to_str().unwrap()])
            .output()
            .expect("create initial order");
        assert_success(&create);
        let order = PathBuf::from(String::from_utf8(create.stdout).unwrap().trim());
        let value: Value =
            serde_json::from_slice(&fs::read(&order).expect("read order")).expect("order JSON");
        fs::write(
            state.join("sprint.jsonl"),
            format!(
                "{}\n",
                json!({
                    "ts": DISPATCH_TIME,
                    "kind": "work-dispatched",
                    "fact": {
                        "schema_version": 1,
                        "item_id": ITEM_ID,
                        "order_id": value["order_id"],
                        "unit_name": UNIT,
                        "backend": "systemd",
                        "cost_ceiling_usd": 20,
                        "token_ceiling": 500000
                    },
                    "narration": {}
                })
            ),
        )
        .expect("write dispatched trace");
        Self {
            root,
            state,
            candidate,
            order,
            systemctl,
        }
    }

    fn command(&self) -> Command {
        command(&self.root, &self.state, &self.systemctl)
    }

    fn order_id(&self) -> String {
        serde_json::from_slice::<Value>(&fs::read(&self.order).expect("read order"))
            .expect("order JSON")["order_id"]
            .as_str()
            .expect("order id")
            .to_owned()
    }

    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(self.state.join("sprint.jsonl"))
            .expect("read trace")
            .lines()
            .map(|line| serde_json::from_str(line).expect("trace row"))
            .collect()
    }
}

fn command(root: &TempDir, state: &Path, systemctl: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
    command
        .env_clear()
        .env("HOME", root.path())
        .env("OSTROM_HOME", state)
        .env("PATH", env::var_os("PATH").unwrap_or_default())
        .env("MANDATE_SYSTEMCTL_BIN", systemctl);
    command
}

fn executable(path: &Path, source: &str) {
    fs::write(path, format!("#!/bin/sh\n{source}\n")).expect("write executable");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod executable");
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn stale_missing_order_is_reaped_before_replacement_without_rewriting_trace() {
    let fixture = Fixture::new("exit 4");
    let original = fs::read(fixture.state.join("sprint.jsonl")).expect("original trace");
    let old_order_id = fixture.order_id();
    let output = fixture
        .command()
        .env("MANDATE_NOW_EPOCH", STALE_EPOCH)
        .env("MANDATE_TRACE_TIME", "2026-08-01T01:33:20Z")
        .args(["work-order", "create", fixture.candidate.to_str().unwrap()])
        .output()
        .expect("replace stale order");
    assert_success(&output);
    let trace_bytes = fs::read(fixture.state.join("sprint.jsonl")).expect("updated trace");
    assert!(trace_bytes.starts_with(&original));
    let trace = fixture.trace();
    assert_eq!(trace.len(), 2);
    assert_eq!(trace[1]["kind"], "work-failed");
    assert_eq!(trace[1]["fact"]["order_id"], old_order_id);
    assert_eq!(trace[1]["fact"]["reason"], "stale-order-reaped");
    assert_eq!(trace[1]["fact"]["reaped"], true);
}

#[test]
fn old_but_live_order_is_not_reaped_or_replaced() {
    let fixture =
        Fixture::new("printf '%s\\n' 'ActiveState=active' 'ExecMainCode=' 'ExecMainStatus=0'");
    let original = fs::read(fixture.state.join("sprint.jsonl")).expect("original trace");
    let output = fixture
        .command()
        .env("MANDATE_NOW_EPOCH", STALE_EPOCH)
        .args(["work-order", "create", fixture.candidate.to_str().unwrap()])
        .output()
        .expect("refuse live replacement");
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("prior order is still in flight"));
    assert_eq!(
        fs::read(fixture.state.join("sprint.jsonl")).expect("unchanged trace"),
        original
    );
}

#[test]
fn stale_bound_reaps_when_unit_state_cannot_be_resolved() {
    let fixture = Fixture::new("exit 1");
    let output = fixture
        .command()
        .env("MANDATE_NOW_EPOCH", STALE_EPOCH)
        .args(["work-order", "create", fixture.candidate.to_str().unwrap()])
        .output()
        .expect("replace order after derived TTL");
    assert_success(&output);
    assert_eq!(fixture.trace()[1]["fact"]["reason"], "stale-order-reaped");
}

#[test]
fn clear_names_one_stranded_order_and_refuses_a_live_one() {
    let missing = Fixture::new(
        "printf '%s\\n' 'ActiveState=failed' 'ExecMainCode=exited' 'ExecMainStatus=17'",
    );
    let order_id = missing.order_id();
    let output = missing
        .command()
        .env("MANDATE_NOW_EPOCH", DISPATCH_EPOCH)
        .args(["work-order", "clear", &order_id])
        .output()
        .expect("clear missing unit");
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains(&order_id));
    let trace = missing.trace();
    assert_eq!(trace[1]["fact"]["reason"], "operator-reaped");
    assert_eq!(trace[1]["fact"]["order_id"], order_id);
    assert_eq!(trace[1]["fact"]["exit_code"], 17);

    let live =
        Fixture::new("printf '%s\\n' 'ActiveState=active' 'ExecMainCode=' 'ExecMainStatus=0'");
    let output = live
        .command()
        .env("MANDATE_NOW_EPOCH", STALE_EPOCH)
        .args(["work-order", "clear", ITEM_ID])
        .output()
        .expect("refuse live clear");
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("still running"));
    assert_eq!(live.trace().len(), 1);
}
