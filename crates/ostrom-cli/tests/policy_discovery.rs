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
      "checks": []
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
        let path = self.home.path().join("ostrom.yaml");
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
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostics");
    assert!(stdout.contains("repository-grant"), "{stdout}");
    assert!(
        stdout.contains(
            fixture
                .repository
                .path()
                .join("ostrom.yaml")
                .to_str()
                .expect("UTF-8 path")
        ),
        "{stdout}"
    );
    assert!(stdout.contains("verdict      MERGE"), "{stdout}");
    assert!(stdout.contains("ACTOR PORTABILITY"), "{stdout}");
    assert!(stdout.contains("builder"), "{stdout}");
    assert!(stdout.contains("NON-PORTABLE"), "{stdout}");
    assert!(
        stdout.contains(
            fixture
                .repository
                .path()
                .join("ostrom.yaml")
                .to_str()
                .expect("UTF-8 path")
        ),
        "{stdout}"
    );
    assert!(!stderr.contains("non-portable"), "{stderr}");
}

#[test]
fn explain_attributes_actor_portability_to_the_included_source() {
    let fixture = RepositoryFixture::new("");
    let included = fixture.repository.path().join("included-actor.yaml");
    fs::write(&included, "actor: gatekeeper\n").expect("write included actor");
    fs::write(
        fixture.repository.path().join("ostrom.yaml"),
        policy("includes: [included-actor.yaml]\n"),
    )
    .expect("write including manifest");
    support::sign_manifest(&fixture.repository.path().join("ostrom.yaml"));

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    let portability = stdout
        .split_once("ACTOR PORTABILITY")
        .map(|(_, section)| section)
        .expect("actor portability section");
    assert!(portability.contains("gatekeeper"), "{portability}");
    assert!(
        portability.contains(included.to_str().expect("UTF-8 path")),
        "{portability}"
    );
}

#[test]
fn repository_deny_beats_operator_grant() {
    let fixture =
        RepositoryFixture::new("denies:\n  repository-veto: {actors: builder, operations: work}\n");
    fixture.write_overlay(
        "actors: {builder: {}}\noperations: {work: {steps: []}}\ngrants:\n  operator-grant: {actors: builder, operations: work}\n",
    );

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    assert!(stdout.contains("repository-veto"), "{stdout}");
    assert!(stdout.contains("operator-grant"), "{stdout}");
    assert!(stdout.contains("verdict      HOLD"), "{stdout}");
}

#[test]
fn no_matching_rule_defaults_to_deny_and_names_every_consulted_scope() {
    let fixture = RepositoryFixture::new("");
    fixture.write_overlay("");

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    assert!(stdout.contains("default deny"), "{stdout}");
    assert!(stdout.contains("SCOPES CONSULTED"), "{stdout}");
    assert!(stdout.contains("repository"), "{stdout}");
    assert!(stdout.contains("operator"), "{stdout}");
    assert!(stdout.contains("verdict      HOLD"), "{stdout}");
}

#[test]
fn repository_loop_is_inert_and_explain_reports_declared_but_not_adopted() {
    let fixture = RepositoryFixture::new(
        "grants:\n  repository-work: {actors: builder, operations: work}\nloops:\n  repository-loop: {actor: builder, operation: work, target: placeholder-org/repository, every: hourly}\n",
    );
    fixture.write_overlay("");
    let rendered = fixture.home.path().join("systemd");
    let render = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .current_dir(fixture.repository.path())
        .env("OSTROM_HOME", fixture.home.path())
        .env("OSTROM_POLICY_TRUSTED_KEYS", &fixture.trusted_keys)
        .args(["loops", "render", "--output"])
        .arg(&rendered)
        .output()
        .expect("render adopted loops");
    assert!(
        render.status.success(),
        "{}",
        String::from_utf8_lossy(&render.stderr)
    );
    assert!(render.stdout.is_empty(), "repository loop must not render");

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    assert!(stdout.contains("repository-loop"), "{stdout}");
    assert!(stdout.contains("DECLARED BUT NOT ADOPTED"), "{stdout}");
}

#[test]
fn yaml_and_yml_resolve_alone_but_both_are_ambiguous() {
    let fixture = RepositoryFixture::new(
        "grants:\n  repository-grant: {actors: builder, operations: work}\n",
    );
    let yaml = fixture.repository.path().join("ostrom.yaml");
    let yml = fixture.repository.path().join("ostrom.yml");
    fs::rename(&yaml, &yml).expect("rename manifest extension");
    fs::rename(
        fixture.repository.path().join("ostrom.yaml.sig"),
        fixture.repository.path().join("ostrom.yml.sig"),
    )
    .expect("rename signature extension");
    let yml_output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(yml_output.status.success());

    fs::copy(&yml, &yaml).expect("add ambiguous yaml path");
    let ambiguous = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(!ambiguous.status.success());
    let stderr = String::from_utf8(ambiguous.stderr).expect("UTF-8 error");
    assert!(
        stderr.contains(yaml.to_str().expect("UTF-8 yaml path")),
        "{stderr}"
    );
    assert!(
        stderr.contains(yml.to_str().expect("UTF-8 yml path")),
        "{stderr}"
    );
}

#[test]
fn operator_yaml_and_yml_resolve_alone_but_both_are_ambiguous() {
    let fixture = RepositoryFixture::new("");
    fixture.write_overlay(
        "actors: {builder: {}}\noperations: {work: {steps: []}}\ngrants:\n  operator-grant: {actors: builder, operations: work}\n",
    );
    let yaml = fixture.home.path().join("ostrom.yaml");
    let yml = fixture.home.path().join("ostrom.yml");
    fs::rename(&yaml, &yml).expect("rename operator manifest extension");
    fs::rename(
        fixture.home.path().join("ostrom.yaml.sig"),
        fixture.home.path().join("ostrom.yml.sig"),
    )
    .expect("rename operator signature extension");
    let yml_output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(yml_output.status.success());

    fs::copy(&yml, &yaml).expect("add ambiguous operator yaml path");
    let ambiguous = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(!ambiguous.status.success());
    let stderr = String::from_utf8(ambiguous.stderr).expect("UTF-8 error");
    assert!(
        stderr.contains(yaml.to_str().expect("UTF-8 yaml path")),
        "{stderr}"
    );
    assert!(
        stderr.contains(yml.to_str().expect("UTF-8 yml path")),
        "{stderr}"
    );
}

#[test]
fn explain_preserves_the_included_file_as_the_rule_origin() {
    let fixture = RepositoryFixture::new("");
    let included = fixture
        .repository
        .path()
        .join(".ostrom/included-grant.yaml");
    fs::create_dir(fixture.repository.path().join(".ostrom")).expect("create include directory");
    fs::write(
        &included,
        "grant: included-grant\nactors: builder\noperations: work\n",
    )
    .expect("write included grant");
    fs::write(
        fixture.repository.path().join("ostrom.yaml"),
        policy("includes: [.ostrom/included-grant.yaml]\n"),
    )
    .expect("write including manifest");
    support::sign_manifest(&fixture.repository.path().join("ostrom.yaml"));

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    assert!(stdout.contains("included-grant"), "{stdout}");
    assert!(
        stdout.contains(included.to_str().expect("UTF-8 include path")),
        "{stdout}"
    );
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

/// A repository whose only policy sits at the retired `.ostrom/manifest.yml`
/// is ungoverned, not governed by the old shape. A deprecation notice is still
/// a loading path: it keeps a second schema alive and keeps its grants in
/// force. The entrypoint is `ostrom.yaml`.
#[test]
fn a_legacy_repository_manifest_does_not_govern() {
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

    assert!(
        !output.status.success(),
        "a legacy manifest must not resolve"
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(
        !stdout.contains("legacy-grant"),
        "a retired manifest must not carry authority: {stdout}"
    );
    assert!(
        !stderr.contains("deprecated"),
        "there is no deprecation path left to announce: {stderr}"
    );
    assert!(stderr.contains("ungoverned"), "{stderr}");
}

#[test]
fn operator_grant_is_accepted_and_resolves_granted() {
    let fixture = RepositoryFixture::new("");
    fixture.write_overlay("grants:\n  forbidden-authority: {actors: builder, operations: work}\n");

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    assert!(stdout.contains("operator"), "{stdout}");
    assert!(stdout.contains("grants.forbidden-authority"), "{stdout}");
    assert!(stdout.contains("verdict      MERGE"), "{stdout}");
}

#[test]
fn operator_deny_applies_and_explain_attributes_the_scope_and_file() {
    let fixture = RepositoryFixture::new("");
    fixture.write_overlay("denies:\n  operator-veto: {actors: builder, operations: work}\n");

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    assert!(stdout.contains("operator"), "{stdout}");
    assert!(stdout.contains("operator-veto"), "{stdout}");
    assert!(stdout.contains("operator denies.operator-veto"), "{stdout}");
    assert!(
        stdout.contains(
            fixture
                .home
                .path()
                .join("ostrom.yaml")
                .to_str()
                .expect("UTF-8 path")
        ),
        "{stdout}"
    );
    assert!(stdout.contains("verdict      HOLD"), "{stdout}");
}

#[test]
fn operator_deny_beats_repository_grant_and_both_are_explained() {
    let fixture = RepositoryFixture::new(
        "grants:\n  repository-grant: {actors: builder, operations: work}\n",
    );
    fixture.write_overlay("denies:\n  overlay-deny: {actors: builder, operations: work}\n");

    let output = fixture.explain_from(fixture.repository.path(), &[]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 explanation");
    assert!(stdout.contains("repository-grant"), "{stdout}");
    assert!(stdout.contains("overlay-deny"), "{stdout}");
    assert!(stdout.contains("operator denies.overlay-deny"), "{stdout}");
    assert!(stdout.contains("verdict      HOLD"), "{stdout}");
}

#[test]
fn changing_the_overlay_after_signing_is_refused() {
    let fixture = RepositoryFixture::new("");
    fixture.write_overlay("denies:\n  operator-veto: {actors: builder, operations: work}\n");
    fs::write(
        fixture.home.path().join("ostrom.yaml"),
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

/// `~/.claude/ostrom/config.yaml` and `./.ostrom/config.yaml` are the touch
/// log's user and repository layers, documented in `skills/touch/SKILL.md`.
/// Claiming `config.yaml` as a legacy policy manifest made `ostrom operations`
/// fail on a real operator's machine with `unknown field `provider``, and the
/// deprecation notice advised moving the file to `ostrom.yaml`, which would
/// have destroyed the touch configuration. The policy loader must not read
/// another subsystem's file.
#[test]
fn a_touch_log_config_is_not_claimed_as_an_operator_policy_manifest() {
    let home = tempdir().expect("temporary OSTROM_HOME");
    let repository = tempdir().expect("temporary repository");
    fs::create_dir(repository.path().join(".git")).expect("repository boundary");
    fs::write(
        home.path().join("config.yaml"),
        "# ostrom \u{2014} touch-log config (user layer)\nprovider: notion\nnotion:\n  data_source: placeholder\n",
    )
    .expect("write touch log config");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .current_dir(repository.path())
        .env("OSTROM_HOME", home.path())
        .arg("operations")
        .output()
        .expect("run operations without an operator manifest");

    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(
        !stderr.contains("config.yaml"),
        "the touch log config must not be named as a policy manifest: {stderr}"
    );
    assert!(
        !stderr.contains("unknown field `provider`"),
        "the touch log config must not be parsed as policy: {stderr}"
    );
    assert!(
        stderr.contains("no adopting operator manifest found"),
        "an absent operator manifest must be named as absent: {stderr}"
    );
}
