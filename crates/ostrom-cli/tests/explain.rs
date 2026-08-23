use std::{fs, path::PathBuf, process::Command};

use tempfile::TempDir;

mod support;

fn fixture_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/policy-explain")
}

struct ExplainFixture {
    home: TempDir,
    policy: TempDir,
    trusted_keys: PathBuf,
}

impl ExplainFixture {
    fn new() -> Self {
        let home = TempDir::new().expect("temporary OSTROM_HOME");
        fs::write(
            home.path().join("state.json"),
            r#"{
  "version": 2,
  "repos": {},
  "policy_holds": {
    "onsager-ai/ostrom#344": {
      "first_held": "2026-08-10T00:00:00Z"
    }
  }
}"#,
        )
        .expect("write held-state fixture");
        let policy = support::copy_fixture_directory(&fixture_source());
        let trusted_keys = support::sign_manifest(&policy.path().join("manifest.yml"));
        Self {
            home,
            policy,
            trusted_keys,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.policy.path().join(name)
    }

    fn explain(&self, target: &str) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_ostrom"))
            .env("OSTROM_HOME", self.home.path())
            .env("OSTROM_POLICY_TRUSTED_KEYS", &self.trusted_keys)
            .args([
                "explain",
                target,
                "--manifest",
                self.path("manifest.yml")
                    .to_str()
                    .expect("UTF-8 fixture path"),
                "--fixture",
                self.path("github.json")
                    .to_str()
                    .expect("UTF-8 fixture path"),
                "--started-at",
                "2026-08-20T00:00:00Z",
            ])
            .output()
            .expect("run ostrom explain")
    }
}

#[test]
fn explain_names_the_verdict_rule_requirement_and_ladder_source() {
    let output = ExplainFixture::new().explain("onsager-ai/ostrom#352");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explain output");
    for expected in [
        "SUBJECT RULES",
        "ACTOR RULES (builder / work)",
        "R-rust-green",
        "decide       builder",
        "grants.R-rust-green",
        "requires     rust-green",
        "PASS",
        "checks.rust-green: gh/check-run name=placeholder-ci",
        "verdict      MERGE",
    ] {
        assert!(stdout.contains(expected), "missing {expected:?}:\n{stdout}");
    }
}

#[test]
fn explain_reports_the_principal_floor_when_no_rule_matches() {
    let output = ExplainFixture::new().explain("onsager-ai/ostrom#353");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explain output");
    assert!(stdout.contains("decide       principal"), "{stdout}");
    assert!(
        stdout.contains("default deny (no grant matched"),
        "{stdout}"
    );
    assert!(
        stdout.contains("no rule granted this pull request; principal is the floor"),
        "{stdout}"
    );
    assert!(stdout.contains("verdict      HOLD"), "{stdout}");
    assert!(stdout.contains("unmatched: warn"), "{stdout}");
    assert!(stdout.contains("unmatched: block"), "{stdout}");
}

#[test]
fn explain_marks_a_recorded_hold_with_its_age_and_rule_override() {
    let output = ExplainFixture::new().explain("onsager-ai/ostrom#344");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explain output");
    for expected in [
        "R-plugin-manifest",
        "denies.R-plugin-manifest",
        "held         10d of 7d  STALLED",
        "denies.R-plugin-manifest.stalls_after",
        "verdict      HOLD",
    ] {
        assert!(stdout.contains(expected), "missing {expected:?}:\n{stdout}");
    }
}
