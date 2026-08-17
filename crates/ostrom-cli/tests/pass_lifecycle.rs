#![cfg(unix)]

use std::{
    fs,
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        command
            .args(["pass", "builder"])
            .env("OSTROM_HOME", &self.state)
            .env("CLAUDE_CONFIG_DIR", self.root.path())
            .env("CLAUDE_BIN", &self.claude)
            .env("MANDATE_TRACE_TIME", "2026-08-01T00:00:00Z")
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
