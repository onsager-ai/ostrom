#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::PathBuf,
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

use chrono::Local;
use tempfile::TempDir;

mod support;

struct Fixture {
    _root: TempDir,
    home: PathBuf,
    repository: PathBuf,
    manifest: PathBuf,
    trusted_keys: PathBuf,
    marker: PathBuf,
}

impl Fixture {
    fn new(spend_usd: f64) -> Self {
        let root = TempDir::new().expect("temporary loop supervisor fixture");
        let home = root.path().join("home");
        let repository = root.path().join("repository");
        let marker = root.path().join("operation-ran");
        fs::create_dir_all(&home).expect("create home");
        fs::create_dir_all(repository.join(".git")).expect("create repository boundary");
        let minute = Local::now().format("%M");
        let operator = format!(
            r#"manifest_version: 1
defaults:
  loop: {{concurrent: 6, spend_usd: {spend_usd}, tokens: 200000}}
actors: {{builder: {{}}}}
operations:
  scheduled-work:
    steps:
      - uses: cmd/run
        with:
          script: 'printf "%s|%s|%s|%s\n" "$OSTROM_ACTOR" "$MANDATE_DAILY_CAP_USD" "$MANDATE_MAX_IMPLEMENTERS" "$MANDATE_ORDER_TOKEN_CEILING" > "$OSTROM_LOOP_MARKER"; printf "worker-log\n"'
loops:
  builder-day:
    actor: builder
    operation: scheduled-work
    target: placeholder-org/repository
    every: "*:{minute}"
"#
        );
        fs::write(home.join("ostrom.yaml"), operator).expect("write operator manifest");
        support::sign_manifest(&home.join("ostrom.yaml"));
        let manifest = repository.join("ostrom.yaml");
        fs::write(
            &manifest,
            "manifest_version: 1\ngrants:\n  scheduled: {actors: builder, operations: scheduled-work, repositories: placeholder-org/repository}\n",
        )
        .expect("write repository manifest");
        let trusted_keys = support::sign_manifest(&manifest);
        Self {
            _root: root,
            home,
            repository,
            manifest,
            trusted_keys,
            marker,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        command
            .current_dir(&self.repository)
            .env("OSTROM_HOME", &self.home)
            .env("OSTROM_POLICY_TRUSTED_KEYS", &self.trusted_keys)
            .env("OSTROM_LOOP_MARKER", &self.marker)
            .env_remove("OSTROM_ACTOR")
            .env_remove("MANDATE_DAILY_CAP_USD")
            .env_remove("MANDATE_MAX_IMPLEMENTERS")
            .env_remove("MANDATE_ORDER_TOKEN_CEILING");
        command
    }

    fn compose(&self) -> String {
        let output = self
            .command()
            .arg("compose")
            .arg(&self.manifest)
            .output()
            .expect("compose current policy");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("UTF-8 compose output")
            .split_whitespace()
            .find_map(|word| word.strip_prefix("digest="))
            .map(str::to_owned)
            .expect("compose output has digest")
    }

    fn up(&self) -> Output {
        self.command().arg("up").output().expect("run ostrom up")
    }

    fn wait_for_marker(&self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !self.marker.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(self.marker.exists(), "loop operation did not run");
    }

    fn make_version_writable(&self, digest: &str) {
        let directory = self.home.join("versions").join(digest);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755))
            .expect("make version directory writable");
        fs::set_permissions(
            directory.join("ostrom.yaml"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("make version manifest writable");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let Ok(entries) = fs::read_dir(self.home.join("versions")) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o755));
            let _ =
                fs::set_permissions(path.join("ostrom.yaml"), fs::Permissions::from_mode(0o644));
        }
    }
}

#[test]
fn up_twice_launches_one_activation_and_applies_manifest_ceilings() {
    let fixture = Fixture::new(50.0);
    fixture.compose();

    let first = fixture.up();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("started=1"));
    fixture.wait_for_marker();

    let second = fixture.up();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(stdout.contains("started=0"), "{stdout}");
    assert!(stdout.contains("unchanged=1"), "{stdout}");
    assert_eq!(
        fs::read_to_string(&fixture.marker).expect("read marker"),
        "builder|50|6|200000\n"
    );

    let logs = fixture
        .command()
        .args(["logs", "builder-day"])
        .output()
        .expect("read loop logs");
    assert!(logs.status.success());
    assert!(String::from_utf8_lossy(&logs.stdout).contains("worker-log"));
}

#[test]
fn an_exceeded_ceiling_stops_before_the_operation() {
    let fixture = Fixture::new(10.0);
    fixture.compose();
    let day = chrono::Utc::now().format("%Y-%m-%d");
    fs::write(
        fixture.home.join("sprint.jsonl"),
        format!(
            "{{\"ts\":\"{day}T00:00:00Z\",\"kind\":\"pass-ended\",\"fact\":{{\"cost_usd\":10}},\"narration\":{{}}}}\n"
        ),
    )
    .expect("write measured consumption");

    let output = fixture.up();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("stopped=1"));
    assert!(!fixture.marker.exists(), "operation ran after its ceiling");
    let state = fs::read_to_string(fixture.home.join("loop-runs/builder-day.json"))
        .expect("read stopped state");
    assert!(state.contains("ceiling_exceeded:spend_usd"), "{state}");
}

#[test]
fn an_unsigned_unit_for_an_undeclared_loop_is_inert() {
    let fixture = Fixture::new(50.0);
    fixture.compose();
    let rogue_marker = fixture.home.join("rogue-ran");
    let unit_dir = fixture.home.join("systemd");
    fs::create_dir(&unit_dir).expect("create unit source");
    fs::write(
        unit_dir.join("ostrom-loop-rogue.service"),
        format!(
            "[Service]\nType=oneshot\nExecStart=sh -c 'touch {}'\n",
            rogue_marker.display()
        ),
    )
    .expect("write rogue unit");

    let output = fixture.up();

    assert!(output.status.success());
    fixture.wait_for_marker();
    assert!(!rogue_marker.exists(), "an unsigned unit was executed");
    assert!(!fixture.home.join("loop-runs/rogue.json").exists());
}

#[test]
fn up_without_a_current_version_refuses_with_a_named_cause() {
    let fixture = Fixture::new(50.0);

    let output = fixture.up();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("current_missing"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.marker.exists());
}

#[test]
fn up_refuses_a_drifted_current_version_and_names_the_drift() {
    let fixture = Fixture::new(50.0);
    let digest = fixture.compose();
    fixture.make_version_writable(&digest);
    let manifest = fixture
        .home
        .join("versions")
        .join(digest)
        .join("ostrom.yaml");
    let mut source = fs::read_to_string(&manifest).expect("read materialized manifest");
    source.push_str("# drift\n");
    fs::write(&manifest, source).expect("drift current manifest");

    let output = fixture.up();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("current_drift"), "{stderr}");
    assert!(stderr.contains("ostrom.yaml"), "{stderr}");
    assert!(!fixture.marker.exists());
}

#[test]
fn ps_distinguishes_zero_spend_from_unmeasured_spend() {
    let fixture = Fixture::new(50.0);
    fixture.compose();

    let zero = fixture.command().arg("ps").output().expect("read zero ps");
    assert!(zero.status.success());
    let zero = String::from_utf8(zero.stdout).expect("UTF-8 ps");
    assert!(zero.contains("$0.00/$50"), "{zero}");

    let day = chrono::Utc::now().format("%Y-%m-%d");
    fs::write(
        fixture.home.join("sprint.jsonl"),
        format!(
            "{{\"ts\":\"{day}T00:00:00Z\",\"kind\":\"pass-ended\",\"fact\":{{\"cost_usd\":null}},\"narration\":{{}}}}\n"
        ),
    )
    .expect("write unmeasured spend");

    let unknown = fixture
        .command()
        .arg("ps")
        .output()
        .expect("read unknown ps");
    assert!(unknown.status.success());
    let unknown = String::from_utf8(unknown.stdout).expect("UTF-8 ps");
    assert!(
        unknown.contains("unknown:spend_not_measured/$50"),
        "{unknown}"
    );
    assert!(!unknown.contains("$0.00/$50"), "{unknown}");
}

#[test]
fn loops_render_writes_the_boot_unit_without_installing_or_activating_it() {
    let fixture = Fixture::new(50.0);
    let output = fixture.home.join("rendered-units");

    let rendered = fixture
        .command()
        .args(["loops", "render", "--output"])
        .arg(&output)
        .output()
        .expect("render loop units");

    assert!(
        rendered.status.success(),
        "{}",
        String::from_utf8_lossy(&rendered.stderr)
    );
    let boot = fs::read_to_string(output.join("ostrom-up.service")).expect("read boot unit");
    assert!(boot.contains("Type=oneshot\n"));
    assert!(boot.contains("ExecStart=ostrom up\n"));
    assert!(!boot.contains("systemctl"));
    assert!(!fixture.home.join("loop-runs").exists());
}
