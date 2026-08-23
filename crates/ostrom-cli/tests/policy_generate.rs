use std::{fs, process::Command};

use ostrom_core::PolicyManifest;
use tempfile::TempDir;

mod support;

fn ostrom() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ostrom"))
}

fn operator_policy() -> &'static str {
    r#"manifest_version: 1
actors: {builder: {}}
checks:
  placeholder-green:
    uses: gh/check-run
    with: {name: placeholder-ci}
operations: {work: {steps: []}}
grants:
  target-grant:
    actors: builder
    operations: work
    repositories: placeholder-org/target
    where: label:delegated
    requires: placeholder-green
  other-grant:
    actors: builder
    operations: work
    repositories: placeholder-org/other
denies:
  protected-path:
    actors: builder
    operations: work
    repositories: placeholder-org/target
    where: path:protected/**
loops:
  target-loop: {actor: builder, operation: work, target: placeholder-org/target, every: hourly}
  other-loop: {actor: builder, operation: work, target: placeholder-org/other, every: hourly}
"#
}

#[test]
fn generate_writes_a_loadable_portable_repository_manifest_to_path_or_stdout() {
    let home = TempDir::new().expect("temporary operator home");
    let operator = home.path().join("ostrom.yaml");
    fs::write(&operator, operator_policy()).expect("write operator policy");
    let trusted_keys = support::sign_manifest(&operator);

    let stdout = ostrom()
        .env("OSTROM_HOME", home.path())
        .env("OSTROM_POLICY_TRUSTED_KEYS", &trusted_keys)
        .args(["generate", "placeholder-org/target"])
        .output()
        .expect("generate policy to stdout");
    assert!(
        stdout.status.success(),
        "{}",
        String::from_utf8_lossy(&stdout.stderr)
    );

    let repository = TempDir::new().expect("temporary repository");
    fs::create_dir(repository.path().join(".git")).expect("repository boundary");
    let generated_path = repository.path().join("ostrom.yaml");
    let written = ostrom()
        .env("OSTROM_HOME", home.path())
        .env("OSTROM_POLICY_TRUSTED_KEYS", &trusted_keys)
        .args(["generate", "placeholder-org/target", "--output"])
        .arg(&generated_path)
        .output()
        .expect("generate policy to path");
    assert!(
        written.status.success(),
        "{}",
        String::from_utf8_lossy(&written.stderr)
    );
    assert!(written.stdout.is_empty());
    assert_eq!(
        stdout.stdout,
        fs::read(&generated_path).expect("read output")
    );

    let yaml = String::from_utf8(stdout.stdout).expect("UTF-8 generated policy");
    let generated = PolicyManifest::parse_yaml(&yaml).expect("generated policy parses");
    assert!(generated.actors.is_empty());
    assert!(generated.grants.contains_key("target-grant"));
    assert!(!generated.grants.contains_key("other-grant"));
    assert!(generated.denies.contains_key("protected-path"));
    assert!(generated.checks.contains_key("placeholder-green"));
    assert!(generated.operations.contains_key("work"));
    assert!(generated.loops.contains_key("target-loop"));
    assert!(!generated.loops.contains_key("other-loop"));
    assert!(
        generated
            .grants
            .values()
            .chain(generated.denies.values())
            .all(|rule| rule.repositories.is_empty())
    );

    support::sign_manifest(&generated_path);
    fs::write(
        repository.path().join("github.json"),
        r#"{
  "repositories": [{
    "repo": "placeholder-org/target",
    "open_prs": [{
      "number": 1,
      "title": "feat: placeholder",
      "labels": [{"name": "delegated"}],
      "files": [{"path": "src/lib.rs"}],
      "statusCheckRollup": [{"name": "placeholder-ci", "conclusion": "SUCCESS"}]
    }]
  }]
}"#,
    )
    .expect("write pull-request fixture");
    let explained = ostrom()
        .current_dir(repository.path())
        .env("OSTROM_HOME", home.path())
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted_keys)
        .args(["explain", "placeholder-org/target#1", "--fixture"])
        .arg(repository.path().join("github.json"))
        .output()
        .expect("load generated repository policy");
    assert!(
        explained.status.success(),
        "generated manifest must load as the repository layer: {}",
        String::from_utf8_lossy(&explained.stderr)
    );
    let explanation = String::from_utf8(explained.stdout).expect("UTF-8 explanation");
    assert!(explanation.contains("verdict      MERGE"), "{explanation}");
    assert!(!explanation.contains("ACTOR PORTABILITY"), "{explanation}");
    assert!(!String::from_utf8_lossy(&explained.stderr).contains("non-portable"));
}

#[test]
fn generate_rejects_a_repository_without_owner_name_shape() {
    let home = TempDir::new().expect("temporary operator home");
    let output = ostrom()
        .env("OSTROM_HOME", home.path())
        .args(["generate", "missing-owner"])
        .output()
        .expect("reject malformed repository");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("owner/name"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
