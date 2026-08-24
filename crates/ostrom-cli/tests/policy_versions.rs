#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;

mod support;

const OPERATOR: &str = concat!(
    "manifest_version: 1\n",
    "actors: {builder: {permission_mode: manual}}\n",
    "operations: {work: {steps: []}}\n",
);

struct Fixture {
    _root: TempDir,
    home: PathBuf,
    repository: PathBuf,
    manifest: PathBuf,
    trusted_keys: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().expect("temporary policy-version fixture");
        let home = root.path().join("home");
        let repository = root.path().join("repository");
        fs::create_dir_all(&home).expect("create operator home");
        fs::create_dir_all(repository.join(".git")).expect("create repository boundary");
        fs::write(home.join("ostrom.yaml"), OPERATOR).expect("write operator manifest");
        let manifest = repository.join("ostrom.yaml");
        fs::write(&manifest, Self::repository_policy("delegated"))
            .expect("write repository manifest");
        let trusted_keys = support::sign_manifest(&manifest);
        support::sign_manifest(&home.join("ostrom.yaml"));
        Self {
            _root: root,
            home,
            repository,
            manifest,
            trusted_keys,
        }
    }

    fn repository_policy(rule: &str) -> String {
        format!("manifest_version: 1\ngrants:\n  {rule}: {{actors: builder, operations: work}}\n")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        command
            .current_dir(&self.repository)
            .env("OSTROM_HOME", &self.home)
            .env("OSTROM_POLICY_TRUSTED_KEYS", &self.trusted_keys);
        command
    }

    fn compose(&self) -> Output {
        self.command()
            .args(["compose"])
            .arg(&self.manifest)
            .output()
            .expect("run ostrom compose")
    }

    fn compose_digest(&self) -> String {
        let output = self.compose();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 compose output");
        stdout
            .split_whitespace()
            .find_map(|word| word.strip_prefix("digest="))
            .map(str::to_owned)
            .expect("compose output names digest")
    }

    fn write_repository_policy(&self, rule: &str) {
        fs::write(&self.manifest, Self::repository_policy(rule))
            .expect("replace repository manifest");
        support::sign_manifest(&self.manifest);
    }

    fn write_operator_permission_mode(&self, permission_mode: &str) {
        let manifest = self.home.join("ostrom.yaml");
        fs::write(
            &manifest,
            format!(
                "manifest_version: 1\nactors: {{builder: {{permission_mode: {permission_mode}}}}}\noperations: {{work: {{steps: []}}}}\n"
            ),
        )
        .expect("replace operator permission mode");
        support::sign_manifest(&manifest);
    }

    fn current_target(&self) -> PathBuf {
        fs::read_link(self.home.join("current")).expect("read current version pointer")
    }

    fn materialized_manifest(&self, digest: &str) -> PathBuf {
        self.home.join("versions").join(digest).join("ostrom.yaml")
    }

    fn make_version_writable(&self, digest: &str) {
        let directory = self.home.join("versions").join(digest);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755))
            .expect("make fixture version directory writable");
        fs::set_permissions(
            directory.join("ostrom.yaml"),
            fs::Permissions::from_mode(0o644),
        )
        .expect("make fixture manifest writable");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let versions = self.home.join("versions");
        let Ok(entries) = fs::read_dir(versions) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o755));
                let _ = fs::set_permissions(
                    path.join("ostrom.yaml"),
                    fs::Permissions::from_mode(0o644),
                );
            }
        }
    }
}

/// The #397 misclassification happened inside the window where policy was
/// half-applied. Validation failing is the easy case — it fails before anything
/// is written. This is the hard one: the manifest is valid, composition has
/// begun, and materialization dies partway. `current` must still resolve to the
/// version that was already serving, and it must still be readable.
#[test]
fn a_compose_that_dies_during_materialization_never_becomes_current() {
    let fixture = Fixture::new();
    let digest = fixture.compose_digest();
    let serving = fixture.current_target();
    fixture.write_repository_policy("delegated-two");

    // Deny writes to the version store so materialization fails after the
    // manifest has already composed and verified cleanly.
    let versions = fixture.home.join("versions");
    let restore = fs::metadata(&versions)
        .expect("read version store mode")
        .permissions();
    fs::set_permissions(&versions, fs::Permissions::from_mode(0o500))
        .expect("seal the version store");

    let output = fixture.compose();

    fs::set_permissions(&versions, restore).expect("unseal the version store");

    assert!(
        !output.status.success(),
        "a compose that cannot materialize must not report success"
    );
    assert_eq!(
        fixture.current_target(),
        serving,
        "`current` moved even though the new version was never completed"
    );
    assert_eq!(serving, Path::new("versions").join(&digest));

    let verify = fixture
        .command()
        .args(["config", "verify"])
        .output()
        .expect("run ostrom config verify");
    assert!(
        String::from_utf8_lossy(&verify.stdout).starts_with("pass"),
        "the surviving version must still verify: {}",
        String::from_utf8_lossy(&verify.stdout)
    );
}

#[test]
fn a_validation_failure_leaves_the_previous_version_serving() {
    let fixture = Fixture::new();
    let digest = fixture.compose_digest();
    let serving = fixture.current_target();
    fs::write(
        &fixture.manifest,
        "manifest_version: 1\ngrants:\n  invalid: {actors: absent, operations: work}\n",
    )
    .expect("write invalid repository policy");
    support::sign_manifest(&fixture.manifest);

    let output = fixture.compose();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("invalid policy manifest"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.current_target(), serving);
    assert_eq!(serving, Path::new("versions").join(digest));
}

#[test]
fn identical_inputs_are_deterministic_and_changed_inputs_change_the_digest() {
    let fixture = Fixture::new();
    let first = fixture.compose_digest();
    let second = fixture.compose_digest();
    assert_eq!(first, second);

    fixture.write_repository_policy("protected");
    let changed = fixture.compose_digest();

    assert_ne!(first, changed);
}

#[test]
fn a_hand_edit_is_drift_that_names_the_materialized_file() {
    let fixture = Fixture::new();
    let digest = fixture.compose_digest();
    fixture.make_version_writable(&digest);
    let manifest = fixture.materialized_manifest(&digest);
    let mut source = fs::read_to_string(&manifest).expect("read materialized manifest");
    source.push_str("# hand edit\n");
    fs::write(&manifest, &source).expect("hand-edit materialized manifest");

    let output = fixture
        .command()
        .args(["config", "verify"])
        .output()
        .expect("verify materialized policy");

    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 verify output");
    assert!(stdout.starts_with("fail "), "{stdout}");
    assert!(stdout.contains("ostrom.yaml"), "{stdout}");
    assert_eq!(
        fs::read_to_string(manifest).expect("read drifted manifest"),
        source,
        "verification must not silently correct drift"
    );
}

#[test]
fn verification_distinguishes_drift_from_an_invalid_current_pointer() {
    let fixture = Fixture::new();
    let digest = fixture.compose_digest();
    fixture.make_version_writable(&digest);
    fs::write(fixture.materialized_manifest(&digest), "not yaml: [")
        .expect("corrupt materialized manifest");
    let drift = fixture
        .command()
        .args(["config", "verify"])
        .output()
        .expect("verify drift");
    assert_eq!(drift.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&drift.stdout).starts_with("fail "));

    fs::remove_file(fixture.home.join("current")).expect("remove current symlink");
    fs::write(fixture.home.join("current"), "not a symlink")
        .expect("replace current with invalid pointer shape");
    let unknown = fixture
        .command()
        .args(["config", "verify"])
        .output()
        .expect("verify invalid current pointer");

    assert_eq!(unknown.status.code(), Some(2));
    let stdout = String::from_utf8(unknown.stdout).expect("UTF-8 verify output");
    assert!(
        stdout.starts_with("inconclusive:current_target_invalid "),
        "{stdout}"
    );
}

#[test]
fn rollback_restores_previous_and_reports_both_digests() {
    let fixture = Fixture::new();
    let first = fixture.compose_digest();
    fixture.write_repository_policy("protected");
    let second = fixture.compose_digest();
    assert_eq!(
        fs::read_link(fixture.home.join("previous-version")).expect("read previous pointer"),
        Path::new("versions").join(&first)
    );

    let output = fixture
        .command()
        .arg("rollback")
        .output()
        .expect("run rollback");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 rollback output");
    assert!(stdout.contains(&format!("from={second}")), "{stdout}");
    assert!(stdout.contains(&format!("to={first}")), "{stdout}");
    assert_eq!(fixture.current_target(), Path::new("versions").join(first));
}

#[test]
fn signed_actor_permission_toggle_activates_and_rolls_back_one_pointer() {
    let fixture = Fixture::new();
    let manual = fixture.compose_digest();
    assert_eq!(
        fixture.current_target(),
        Path::new("versions").join(&manual)
    );

    fixture.write_operator_permission_mode("auto");
    let auto = fixture.compose_digest();

    assert_ne!(manual, auto);
    assert_eq!(fixture.current_target(), Path::new("versions").join(&auto));
    assert_eq!(
        fs::read_link(fixture.home.join("previous-version")).expect("read rollback pointer"),
        Path::new("versions").join(&manual)
    );

    let output = fixture
        .command()
        .arg("rollback")
        .output()
        .expect("roll back permission mode");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.current_target(), Path::new("versions").join(manual));
}

#[test]
fn rollback_without_a_previous_version_refuses_with_a_named_cause() {
    let fixture = Fixture::new();
    fixture.compose_digest();

    let output = fixture
        .command()
        .arg("rollback")
        .output()
        .expect("run rollback without previous");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 rollback refusal");
    assert!(stderr.contains("previous_missing"), "{stderr}");
}

#[test]
fn materialized_policy_is_read_only() {
    let fixture = Fixture::new();
    let digest = fixture.compose_digest();
    let mode = fs::metadata(fixture.materialized_manifest(&digest))
        .expect("materialized manifest metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o444);

    let output = fixture
        .command()
        .args(["config", "verify"])
        .output()
        .expect("verify pristine materialized policy");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 verify output"),
        format!("pass digest={digest}\n")
    );
}

#[test]
fn a_missing_version_is_inconclusive_with_a_named_cause() {
    let fixture = Fixture::new();
    let digest = fixture.compose_digest();
    fixture.make_version_writable(&digest);
    fs::remove_file(fixture.materialized_manifest(&digest)).expect("remove materialized manifest");
    fs::remove_dir(fixture.home.join("versions").join(&digest)).expect("remove version directory");

    let output = fixture
        .command()
        .args(["config", "verify"])
        .output()
        .expect("verify missing policy version");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with("inconclusive:version_missing "),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// `previous/` is the sweep's backup directory and predates policy versions.
/// The rollback pointer is `previous-version/` so the two never contend for one
/// name: composing must leave the sweep's backups exactly as it found them,
/// with no migration and no directory displaced.
#[test]
fn composing_does_not_disturb_the_sweep_backup_directory() {
    let fixture = Fixture::new();
    let sweep_backup = fixture.home.join("previous");
    fs::create_dir(&sweep_backup).expect("create sweep backup directory");
    fs::write(sweep_backup.join("queue.jsonl"), "preserved\n").expect("write sweep backup");
    let first = fixture.compose_digest();
    fixture.write_repository_policy("protected");
    fixture.compose_digest();

    assert_eq!(
        fs::read_to_string(sweep_backup.join("queue.jsonl")).expect("read sweep backup"),
        "preserved\n",
        "composing must not move or rewrite the sweep's backup"
    );
    assert!(
        sweep_backup.is_dir() && !sweep_backup.is_symlink(),
        "the sweep backup must remain a plain directory"
    );
    assert_eq!(
        fs::read_link(fixture.home.join("previous-version")).expect("read rollback pointer"),
        Path::new("versions").join(first)
    );
}
