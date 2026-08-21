use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use tempfile::tempdir;

fn ostrom() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ostrom"))
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/loops")
}

#[test]
fn rendered_units_match_the_committed_fixture_and_check_clean() {
    let root = tempdir().expect("temporary render fixture");
    let output = ostrom()
        .args(["loops", "render", "--output"])
        .arg(root.path())
        .env("OSTROM_HOME", root.path())
        .env("OSTROM_POLICY_MANIFEST", fixture().join("policy.yaml"))
        .output()
        .expect("render loop units");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let expected = fixture().join("expected");
    let expected_names = unit_names(&expected);
    assert_eq!(unit_names(root.path()), expected_names);
    for name in expected_names {
        assert_eq!(
            fs::read(root.path().join(&name)).expect("rendered unit"),
            fs::read(expected.join(&name)).expect("expected unit"),
            "{name}"
        );
    }

    let checked = ostrom()
        .args(["loops", "check"])
        .arg(root.path())
        .env("OSTROM_HOME", root.path())
        .env("OSTROM_POLICY_MANIFEST", fixture().join("policy.yaml"))
        .output()
        .expect("check rendered units");
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
}

#[test]
fn unattended_triage_has_its_own_actor_settings_profile() {
    let root = tempdir().expect("temporary settings fixture");
    let output = ostrom()
        .args(["operations", "--settings", "triage"])
        .env("OSTROM_HOME", root.path())
        .env("OSTROM_POLICY_MANIFEST", fixture().join("policy.yaml"))
        .output()
        .expect("render triage settings");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let settings = String::from_utf8(output.stdout).expect("settings are UTF-8");
    assert!(settings.contains("\"OSTROM_ACTOR\": \"triage\""));
    assert!(settings.contains("Bash(ostrom queue-triage *)"));
    assert!(!settings.contains("Bash(ostrom build-pass *)"));
    assert!(!settings.contains("Bash(ostrom gate-pass *)"));
}

#[test]
fn drift_check_refuses_a_hand_edit_without_touching_it() {
    let root = tempdir().expect("temporary drift fixture");
    let unit = root.path().join("ostrom-loop-builder-day.timer");
    fs::write(&unit, "placeholder hand edit\n").expect("write hand edit");
    let before = fs::read(&unit).expect("read before");
    let output = ostrom()
        .args(["loops", "check"])
        .arg(root.path())
        .env("OSTROM_HOME", root.path())
        .env("OSTROM_POLICY_MANIFEST", fixture().join("policy.yaml"))
        .output()
        .expect("check drift");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("changed: ostrom-loop-builder-day.timer"),
        "{stderr}"
    );
    assert_eq!(fs::read(unit).expect("read after"), before);
}

#[test]
fn loop_run_injects_declared_ceilings_and_refuses_an_enforced_mismatch() {
    let root = tempdir().expect("temporary loop run fixture");
    let manifest = root.path().join("policy.yaml");
    let marker = root.path().join("ceilings.txt");
    fs::write(
        &manifest,
        r#"manifest_version: 1
defaults:
  loop: {concurrent: 6, spend_usd: 50, tokens: 200000}
actors: {builder: {}}
operations:
  scheduled-work:
    steps:
      - uses: cmd/run
        with:
          script: 'printf "%s|%s|%s\n" "$MANDATE_DAILY_CAP_USD" "$MANDATE_MAX_IMPLEMENTERS" "$MANDATE_ORDER_TOKEN_CEILING" > "$OSTROM_LOOP_MARKER"'
grants:
  scheduled-work:
    actors: builder
    operations: scheduled-work
    repositories: placeholder-org/repository
loops:
  builder-night:
    actor: builder
    operation: scheduled-work
    target: placeholder-org/repository
    every: ["23:15", "02:15", "05:15"]
    concurrent: 2
"#,
    )
    .expect("write policy fixture");

    let output = loop_run(root.path(), &manifest, &marker)
        .output()
        .expect("run loop");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&marker).expect("ceiling marker"),
        "50|2|200000\n"
    );

    fs::remove_file(&marker).expect("remove marker");
    let mismatch = loop_run(root.path(), &manifest, &marker)
        .env("MANDATE_MAX_IMPLEMENTERS", "9")
        .output()
        .expect("run mismatched loop");
    assert!(!mismatch.status.success());
    let stderr = String::from_utf8_lossy(&mismatch.stderr);
    assert!(
        stderr.contains("ceiling mismatch for `concurrent`"),
        "{stderr}"
    );
    assert!(!marker.exists(), "dispatch must stop before its operation");
}

fn loop_run(root: &Path, manifest: &Path, marker: &Path) -> Command {
    let mut command = ostrom();
    command
        .args(["loop", "run", "builder-night"])
        .env("OSTROM_HOME", root)
        .env("OSTROM_POLICY_MANIFEST", manifest)
        .env("OSTROM_LOOP_MARKER", marker)
        .env_remove("OSTROM_ACTOR")
        .env_remove("MANDATE_DAILY_CAP_USD")
        .env_remove("MANDATE_MAX_IMPLEMENTERS")
        .env_remove("MANDATE_ORDER_TOKEN_CEILING");
    command
}

fn unit_names(directory: &Path) -> Vec<String> {
    let mut names = fs::read_dir(directory)
        .expect("unit fixture directory")
        .map(|entry| {
            entry
                .expect("unit fixture entry")
                .file_name()
                .into_string()
                .expect("UTF-8 fixture name")
        })
        .filter(|name| name.ends_with(".service") || name.ends_with(".timer"))
        .collect::<Vec<_>>();
    names.sort();
    names
}
