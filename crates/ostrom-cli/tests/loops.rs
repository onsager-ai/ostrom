use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use tempfile::{TempDir, tempdir};

mod support;

fn ostrom() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ostrom"))
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/loops")
}

/// A signed copy of the loop fixture. `ostrom` refuses an unsigned manifest,
/// and the signature must not be written beside the checked-in fixture, so the
/// tree is copied into a temporary directory and signed there.
struct SignedFixture {
    _root: TempDir,
    manifest: PathBuf,
    trusted_keys: PathBuf,
}

impl SignedFixture {
    fn new() -> Self {
        let root = support::copy_fixture_directory(&fixture());
        let manifest = root.path().join("policy.yaml");
        let trusted_keys = support::sign_manifest(&manifest);
        Self {
            _root: root,
            manifest,
            trusted_keys,
        }
    }

    fn ostrom(&self) -> Command {
        let mut command = ostrom();
        command
            .env("OSTROM_POLICY_MANIFEST", &self.manifest)
            .env("OSTROM_POLICY_TRUSTED_KEYS", &self.trusted_keys);
        command
    }
}

#[test]
fn rendered_units_match_the_committed_fixture_and_check_clean() {
    let root = tempdir().expect("temporary render fixture");
    let policy = SignedFixture::new();
    let output = policy
        .ostrom()
        .args(["loops", "render", "--output"])
        .arg(root.path())
        .env("OSTROM_HOME", root.path())
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

    let checked = policy
        .ostrom()
        .args(["loops", "check"])
        .arg(root.path())
        .env("OSTROM_HOME", root.path())
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
    let policy = SignedFixture::new();
    let output = policy
        .ostrom()
        .args(["operations", "--settings", "triage"])
        .env("OSTROM_HOME", root.path())
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
    let policy = SignedFixture::new();
    let output = policy
        .ostrom()
        .args(["loops", "check"])
        .arg(root.path())
        .env("OSTROM_HOME", root.path())
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
    let trusted_keys = support::sign_manifest(&manifest);

    let output = loop_run(root.path(), &manifest, &trusted_keys, &marker)
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
    let mismatch = loop_run(root.path(), &manifest, &trusted_keys, &marker)
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

fn loop_run(root: &Path, manifest: &Path, trusted_keys: &Path, marker: &Path) -> Command {
    let mut command = ostrom();
    command
        .args(["loop", "run", "builder-night"])
        .env("OSTROM_HOME", root)
        .env("OSTROM_POLICY_MANIFEST", manifest)
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted_keys)
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

/// Rendering writes unit files; it must never switch one on.
///
/// This is the boundary `dotclaude/systemd/enabled-timers` exists to hold. Its
/// header records why: before it, writing a unit file into that repository was
/// "sufficient to get arbitrary code running on a schedule as this user, within
/// 15 minutes, with nothing in between — and agents write files in this
/// repository."
///
/// `sys/enable-loop` is ungrantable, so no operation can confer enabling. That
/// covers the manifest. This covers the renderer: today it holds because the
/// render path simply contains no activation call, and a property that holds by
/// absence is one a later change removes without noticing.
#[test]
fn the_loop_renderer_never_activates_a_unit() {
    let source = include_str!("../src/main.rs");
    let start = source
        .find("fn run_loops_command")
        .expect("the loops command exists");
    let region = &source[start..];
    let end = region[1..]
        .find("\nfn ")
        .map_or(region.len(), |offset| offset + 1);
    let body = &region[..end];

    for forbidden in [
        "systemctl",
        "enable --now",
        "daemon-reload",
        "MANDATE_SYSTEMCTL_BIN",
    ] {
        assert!(
            !body.contains(forbidden),
            "the loop renderer must not reference `{forbidden}`: rendering a unit \
             and activating one are different authorities, and only the second is \
             the principal's"
        );
    }
}
