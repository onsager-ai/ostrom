#![cfg(unix)]

//! #628: resource admission holds a launch the machine cannot take, and
//! refuses loudly when it cannot read the sensor a declared limit names.
//! Checked before any GitHub call or worktree work (`dispatch.rs`), so these
//! tests need no git repository or credential stub for the held cases.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::{Value, json};
use tempfile::TempDir;

mod support;

struct Fixture {
    root: TempDir,
    state: PathBuf,
    source: PathBuf,
    order_file: PathBuf,
    gh_as: PathBuf,
    codex: PathBuf,
    sys_root: PathBuf,
    proc_root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("admission fixture root");
        let state = root.path().join("ostrom");
        let source = root.path().join("placeholder-source");
        fs::create_dir_all(&state).expect("create state root");
        fs::create_dir_all(&source).expect("create source repository placeholder");

        let order = json!({
            "schema_version": 1,
            "item_id": "placeholder-org/alpha#9",
            "repository": "placeholder-org/alpha",
            "item_ref": "#9",
            "branch_name": "ostrom/9-placeholder",
            "spec": "Change a placeholder fixture.",
            "acceptance_criteria": ["The placeholder changes."],
            "constraints": ["Use placeholder data only."],
            "order_id": "2223456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
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

        let codex = root.path().join("codex-stub");
        executable(&codex, "exit 0");

        let sys_root = root.path().join("sys");
        let proc_root = root.path().join("proc");
        fs::create_dir_all(&sys_root).expect("create fixture sys root");
        fs::create_dir_all(&proc_root).expect("create fixture proc root");

        Self {
            root,
            state,
            source,
            order_file,
            gh_as,
            codex,
            sys_root,
            proc_root,
        }
    }

    /// Compose a signed current policy version declaring `manifest` under
    /// `defaults`, so `ostrom dispatch` reads `defaults.admission` from it.
    fn compose(&self, manifest_defaults: &str) {
        let path = self.state.join("ostrom.yaml");
        fs::write(
            &path,
            format!("manifest_version: 1\ndefaults:\n{manifest_defaults}\n"),
        )
        .expect("write operator manifest");
        let trusted_keys = support::sign_manifest(&path);
        let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
            .current_dir(&self.state)
            .env("OSTROM_HOME", &self.state)
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

    fn dispatch(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        command
            .arg("dispatch")
            .arg(&self.order_file)
            .current_dir(self.root.path())
            .env("OSTROM_HOME", &self.state)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env("OSTROM_PLUGIN_ROOT", plugin_root())
            .env("MANDATE_IMPLEMENTER_SOURCE_REPO", &self.source)
            .env("MANDATE_GH_AS_BIN", &self.gh_as)
            .env("CODEX_BIN", &self.codex)
            .env("MANDATE_ADMISSION_SYS_ROOT", &self.sys_root)
            .env("MANDATE_ADMISSION_PROC_ROOT", &self.proc_root);
        command
    }

    /// A process-backend dispatch that can actually reach a launch: a stub
    /// binary in place of `ostrom implement`, so no git repository or Codex
    /// harness is needed to observe a successful `work-dispatched`.
    fn dispatch_through_a_stub_worker(&self) -> Command {
        let worker = self.root.path().join("implementer-worker-stub");
        executable(&worker, "exit 0");
        let mut command = self.dispatch();
        command
            .env("MANDATE_DISPATCH_BACKEND", "process")
            .env("MANDATE_OSTROM_BIN", &worker)
            .env("MANDATE_IMPLEMENTER_STARTUP_GRACE_MILLISECONDS", "100");
        command
    }

    fn write_coretemp_package(&self, millidegrees_c: i64) {
        write(
            &self.sys_root.join("class/hwmon/hwmon0/temp1_label"),
            "Package id 0\n",
        );
        write(
            &self.sys_root.join("class/hwmon/hwmon0/temp1_input"),
            &format!("{millidegrees_c}\n"),
        );
    }

    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(self.state.join("sprint.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("trace row"))
            .collect()
    }
}

fn plugin_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../ostrom-store/assets")
        .canonicalize()
        .expect("plugin root")
}

fn write(path: &Path, contents: &str) {
    fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
    fs::write(path, contents).expect("write fixture file");
}

fn executable(path: &Path, body: &str) {
    fs::write(path, format!("#!/usr/bin/env bash\nset -eu\n{body}\n")).expect("write stub");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
}

#[test]
fn an_over_limit_reading_holds_admission_and_a_later_reading_under_the_limit_proceeds() {
    let fixture = Fixture::new();
    fixture.compose("  admission:\n    max_cpu_temp_c: 80\n");
    fixture.write_coretemp_package(91_000);

    let held = fixture
        .dispatch_through_a_stub_worker()
        .output()
        .expect("dispatch while over the temperature limit");
    let trace_after_hold = fixture.trace();
    let held_fact = trace_after_hold
        .iter()
        .find(|row| row["kind"] == "admission-held")
        .cloned()
        .unwrap_or(Value::Null);

    assert_eq!(
        json!({
            "refused": !held.status.success(),
            "nothing_dispatched": !trace_after_hold.iter().any(|row| row["kind"] == "work-dispatched"),
            "reason": held_fact["fact"]["reason"],
            "metric": held_fact["fact"]["metric"],
            "limit": held_fact["fact"]["limit"],
            "reading": held_fact["fact"]["reading"],
        }),
        json!({
            "refused": true,
            "nothing_dispatched": true,
            "reason": "over-limit",
            "metric": "max_cpu_temp_c",
            "limit": 80.0,
            "reading": 91.0,
        }),
        "dispatch stderr: {}",
        String::from_utf8_lossy(&held.stderr)
    );

    // The package cools; the same dispatch now proceeds.
    fixture.write_coretemp_package(60_000);
    let proceeded = fixture
        .dispatch_through_a_stub_worker()
        .output()
        .expect("dispatch once the reading is under the limit");
    let trace_after_proceeding = fixture.trace();
    assert!(
        proceeded.status.success(),
        "{}",
        String::from_utf8_lossy(&proceeded.stderr)
    );
    assert!(
        trace_after_proceeding
            .iter()
            .any(|row| row["kind"] == "work-dispatched"),
        "{trace_after_proceeding:?}"
    );
}

#[test]
fn a_declared_limit_with_no_sensor_holds_loudly_and_exits_nonzero() {
    let fixture = Fixture::new();
    fixture.compose("  admission:\n    max_cpu_temp_c: 80\n");
    // sys_root exists but carries no hwmon or thermal_zone entries at all.

    let output = fixture
        .dispatch_through_a_stub_worker()
        .output()
        .expect("dispatch with a declared limit and no sensor");
    let trace = fixture.trace();
    let held_fact = trace
        .iter()
        .find(|row| row["kind"] == "admission-held")
        .cloned()
        .unwrap_or(Value::Null);

    assert_eq!(
        json!({
            "exit_code_nonzero": output.status.code().is_some_and(|code| code != 0),
            "nothing_dispatched": !trace.iter().any(|row| row["kind"] == "work-dispatched"),
            "reason": held_fact["fact"]["reason"],
            "metric": held_fact["fact"]["metric"],
            "reading_is_null": held_fact["fact"]["reading"].is_null(),
        }),
        json!({
            "exit_code_nonzero": true,
            "nothing_dispatched": true,
            "reason": "sensor-unreadable",
            "metric": "max_cpu_temp_c",
            "reading_is_null": true,
        }),
        "dispatch stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn undeclared_limits_with_no_sensors_proceed() {
    let fixture = Fixture::new();
    // No `defaults.admission` composed at all, and no sensor fixtures under
    // sys_root/proc_root: an operator on a machine with no sensors is
    // unaffected (principle 2).

    let output = fixture
        .dispatch_through_a_stub_worker()
        .output()
        .expect("dispatch with no admission declared");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let trace = fixture.trace();
    assert!(
        !trace.iter().any(|row| row["kind"] == "admission-held"),
        "an undeclared limit must never hold: {trace:?}"
    );
    assert!(
        trace.iter().any(|row| row["kind"] == "work-dispatched"),
        "{trace:?}"
    );
}
