#![cfg(unix)]

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use ostrom_core::WorkOrder;
use serde_json::{Value, json};
use tempfile::TempDir;

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
            .env("CLAUDE_PLUGIN_ROOT", plugin_root())
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

fn plugin_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/ostrom")
        .canonicalize()
        .expect("plugin root")
}

fn executable(path: &Path, body: &str) {
    fs::write(path, format!("#!/usr/bin/env bash\nset -eu\n{body}\n")).expect("write stub");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
}
