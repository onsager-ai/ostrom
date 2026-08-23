use std::{fs, path::Path, process::Command};

use tempfile::{TempDir, tempdir};

mod support;

const FIXTURE: &str = r#"{
  "repositories": [{
    "repo": "placeholder-org/repository",
    "open_prs": [{
      "number": 1,
      "title": "feat: layered policy",
      "labels": [],
      "files": [{"path": "src/lib.rs"}],
      "statusCheckRollup": []
    }]
  }]
}"#;

fn policy(rules: &str) -> String {
    format!(
        "manifest_version: 1\nactors: {{builder: {{}}}}\noperations: {{work: {{steps: []}}}}\n{rules}"
    )
}

struct RepositoryFixture {
    repository: TempDir,
    home: TempDir,
    trusted_keys: std::path::PathBuf,
}

impl RepositoryFixture {
    fn new(rules: &str) -> Self {
        let repository = tempdir().expect("temporary repository");
        let home = tempdir().expect("temporary OSTROM_HOME");
        fs::create_dir(repository.path().join(".git")).expect("repository boundary");
        fs::write(repository.path().join("ostrom.yaml"), policy(rules))
            .expect("write repository manifest");
        fs::write(repository.path().join("github.json"), FIXTURE).expect("write fixture");
        let trusted_keys = support::sign_manifest(&repository.path().join("ostrom.yaml"));
        Self {
            repository,
            home,
            trusted_keys,
        }
    }

    fn write_overlay(&self, rules: &str) {
        let path = self.home.path().join("config.yaml");
        fs::write(&path, format!("manifest_version: 1\n{rules}")).expect("write private overlay");
        support::sign_manifest(&path);
    }

    fn explain_from(&self, current_dir: &Path, extra: &[&str]) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        command
            .current_dir(current_dir)
            .env("OSTROM_HOME", self.home.path())
            .env("OSTROM_POLICY_TRUSTED_KEYS", &self.trusted_keys)
            .args(["explain", "placeholder-org/repository#1", "--fixture"])
            .arg(self.repository.path().join("github.json"))
            .args(extra)
            .output()
            .expect("run ostrom explain")
    }
}

#[test]
fn discovers_ostrom_yaml_from_a_nested_working_directory() {
    let fixture = RepositoryFixture::new(
        "grants:\n  repository-grant: {actors: builder, operations: work}\n",
    );
    let nested = fixture.repository.path().join("one/two");
    fs::create_dir_all(&nested).expect("nested working directory");

    let output = fixture.explain_from(&nested, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    assert!(stdout.contains("repository-grant"), "{stdout}");
    assert!(stdout.contains("verdict      MERGE"), "{stdout}");
}

#[test]
fn discovery_stops_at_git_boundary_and_refuses_operator_policy() {
    let outer = tempdir().expect("outer directory");
    let repository = outer.path().join("repository");
    let nested = repository.join("nested");
    let home = tempdir().expect("temporary OSTROM_HOME");
    fs::create_dir_all(repository.join(".git")).expect("repository boundary");
    fs::create_dir(&nested).expect("nested working directory");
    fs::write(
        outer.path().join("ostrom.yaml"),
        policy("grants:\n  outer-grant: {actors: builder, operations: work}\n"),
    )
    .expect("write manifest above boundary");
    fs::write(
        home.path().join("manifest.yml"),
        policy("grants:\n  operator-grant: {actors: builder, operations: work}\n"),
    )
    .expect("write legacy operator manifest");
    fs::write(outer.path().join("github.json"), FIXTURE).expect("write fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .current_dir(&nested)
        .env("OSTROM_HOME", home.path())
        .args(["explain", "placeholder-org/repository#1", "--fixture"])
        .arg(outer.path().join("github.json"))
        .output()
        .expect("run ungoverned explain");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(stderr.contains("ungoverned"), "{stderr}");
    assert!(
        stderr.contains(repository.to_str().expect("UTF-8 path")),
        "{stderr}"
    );
    assert!(!stderr.contains("outer-grant"), "{stderr}");
    assert!(!stderr.contains("operator-grant"), "{stderr}");
}

#[test]
fn explicit_manifest_overrides_discovery() {
    let fixture =
        RepositoryFixture::new("denies:\n  discovered-deny: {actors: builder, operations: work}\n");
    let explicit = fixture.repository.path().join("explicit.yaml");
    fs::write(
        &explicit,
        policy("grants:\n  explicit-grant: {actors: builder, operations: work}\n"),
    )
    .expect("write explicit manifest");
    support::sign_manifest(&explicit);

    let output = fixture.explain_from(
        fixture.repository.path(),
        &["--manifest", explicit.to_str().expect("UTF-8 path")],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    assert!(stdout.contains("explicit-grant"), "{stdout}");
    assert!(!stdout.contains("discovered-deny"), "{stdout}");
    assert!(stdout.contains("verdict      MERGE"), "{stdout}");
}

#[test]
fn legacy_repository_manifest_resolves_with_one_deprecation_notice() {
    let fixture =
        RepositoryFixture::new("grants:\n  legacy-grant: {actors: builder, operations: work}\n");
    let legacy_directory = fixture.repository.path().join(".ostrom");
    fs::create_dir(&legacy_directory).expect("legacy manifest directory");
    fs::rename(
        fixture.repository.path().join("ostrom.yaml"),
        legacy_directory.join("manifest.yml"),
    )
    .expect("move manifest to legacy path");
    fs::rename(
        fixture.repository.path().join("ostrom.yaml.sig"),
        legacy_directory.join("manifest.yml.sig"),
    )
    .expect("move manifest signature to legacy path");

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 warning");
    assert!(stdout.contains("legacy-grant"), "{stdout}");
    assert_eq!(stderr.matches("deprecated").count(), 1, "{stderr}");
    assert!(stderr.contains("ostrom.yaml"), "{stderr}");
}

#[test]
fn overlay_grant_is_refused_and_names_the_rule() {
    let fixture = RepositoryFixture::new("");
    fixture.write_overlay("grants:\n  forbidden-authority: {actors: builder, operations: work}\n");

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(stderr.contains("may not grant"), "{stderr}");
    assert!(stderr.contains("grants.forbidden-authority"), "{stderr}");
}

#[test]
fn repository_loop_is_refused_and_names_the_loop() {
    let fixture = RepositoryFixture::new(
        "grants:\n  loop-work: {actors: builder, operations: work}\nloops:\n  forbidden-loop: {actor: builder, operation: work, target: placeholder-org/repository, every: hourly}\n",
    );

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(stderr.contains("may not declare loops"), "{stderr}");
    assert!(stderr.contains("loops.forbidden-loop"), "{stderr}");
    assert!(
        stderr.contains(
            fixture
                .repository
                .path()
                .join("ostrom.yaml")
                .to_str()
                .expect("UTF-8 path")
        ),
        "{stderr}"
    );
}

#[test]
fn legacy_repository_loop_is_refused_and_names_the_loop() {
    let fixture = RepositoryFixture::new(
        "grants:\n  loop-work: {actors: builder, operations: work}\nloops:\n  legacy-loop: {actor: builder, operation: work, target: placeholder-org/repository, every: hourly}\n",
    );
    let legacy_directory = fixture.repository.path().join(".ostrom");
    fs::create_dir(&legacy_directory).expect("legacy manifest directory");
    fs::rename(
        fixture.repository.path().join("ostrom.yaml"),
        legacy_directory.join("manifest.yml"),
    )
    .expect("move manifest to legacy path");
    fs::rename(
        fixture.repository.path().join("ostrom.yaml.sig"),
        legacy_directory.join("manifest.yml.sig"),
    )
    .expect("move manifest signature to legacy path");

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(stderr.contains("may not declare loops"), "{stderr}");
    assert!(stderr.contains("loops.legacy-loop"), "{stderr}");
    assert!(
        stderr.contains(
            legacy_directory
                .join("manifest.yml")
                .to_str()
                .expect("UTF-8 path")
        ),
        "{stderr}"
    );
}

#[test]
fn operator_loop_resolves_while_a_repository_manifest_is_present() {
    let fixture = RepositoryFixture::new("");
    let operator_manifest = fixture.home.path().join("config.yaml");
    fs::write(
        &operator_manifest,
        policy(
            "grants:\n  loop-work: {actors: builder, operations: work}\nloops:\n  operator-loop: {actor: builder, operation: work, target: placeholder-org/repository, every: hourly}\n",
        ),
    )
    .expect("write operator manifest");
    support::sign_manifest(&operator_manifest);

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .current_dir(fixture.repository.path())
        .env("OSTROM_HOME", fixture.home.path())
        .env("OSTROM_POLICY_MANIFEST", &operator_manifest)
        .env("OSTROM_POLICY_TRUSTED_KEYS", &fixture.trusted_keys)
        .env_remove("OSTROM_ACTOR")
        .env_remove("MANDATE_DAILY_CAP_USD")
        .env_remove("MANDATE_MAX_IMPLEMENTERS")
        .env_remove("MANDATE_ORDER_TOKEN_CEILING")
        .args(["loop", "run", "operator-loop"])
        .output()
        .expect("run operator loop");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn repository_manifest_without_loops_is_unaffected() {
    let fixture = RepositoryFixture::new(
        "grants:\n  repository-grant: {actors: builder, operations: work}\n",
    );

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    assert!(stdout.contains("repository-grant"), "{stdout}");
    assert!(stdout.contains("verdict      MERGE"), "{stdout}");
}

#[test]
fn overlay_deny_applies_and_explain_attributes_the_layer() {
    let fixture = RepositoryFixture::new("");
    fixture.write_overlay("denies:\n  operator-veto: {actors: builder, operations: work}\n");

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    assert!(stdout.contains("overlay"), "{stdout}");
    assert!(stdout.contains("operator-veto"), "{stdout}");
    assert!(stdout.contains("overlay denies.operator-veto"), "{stdout}");
    assert!(stdout.contains("verdict      HOLD"), "{stdout}");
}

#[test]
fn repository_rule_precedes_overlay_rule_and_both_are_explained() {
    let fixture = RepositoryFixture::new(
        "grants:\n  repository-grant: {actors: builder, operations: work}\n",
    );
    fixture.write_overlay("denies:\n  overlay-deny: {actors: builder, operations: work}\n");

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    assert!(stdout.contains("repository-grant"), "{stdout}");
    assert!(stdout.contains("overlay-deny"), "{stdout}");
    assert!(
        stdout.contains("repository grants.repository-grant"),
        "{stdout}"
    );
    assert!(stdout.contains("verdict      MERGE"), "{stdout}");
}

#[test]
fn changing_the_overlay_after_signing_is_refused() {
    let fixture = RepositoryFixture::new("");
    fixture.write_overlay("denies:\n  operator-veto: {actors: builder, operations: work}\n");
    fs::write(
        fixture.home.path().join("config.yaml"),
        "manifest_version: 1\ndenies:\n  changed-veto: {actors: builder, operations: work}\n",
    )
    .expect("tamper with signed overlay");

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(stderr.contains("signature verification failed"), "{stderr}");
}

#[test]
fn sweep_still_runs_in_a_repository_that_has_no_manifest_yet() {
    // Until #364 cuts over, the retired surfaces are still the running system.
    // A repository with no `ostrom.yaml` must not stop the loop — but it must
    // also not silently pick up the operator's own policy on the way past.
    let outer = tempdir().expect("outer directory");
    let repository = outer.path().join("repository");
    let home = tempdir().expect("temporary OSTROM_HOME");
    fs::create_dir_all(repository.join(".git")).expect("repository boundary");
    fs::write(
        home.path().join("manifest.yml"),
        policy("grants:\n  operator-grant: {actors: builder, operations: work}\n"),
    )
    .expect("write legacy operator manifest");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .current_dir(&repository)
        .env("OSTROM_HOME", home.path())
        .args(["sweep"])
        .output()
        .expect("run sweep without a repository manifest");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("ungoverned"),
        "sweep must tolerate an absent manifest before the cutover: {stderr}"
    );
    assert!(
        !stderr.contains("operator-grant"),
        "an absent manifest must not fall through to operator policy: {stderr}"
    );
}
