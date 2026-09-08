#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;

use ostrom_core::PolicyManifest;

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
            .env_remove("OSTROM_POLICY_MANIFEST")
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

#[test]
fn validate_and_compose_agree_on_acceptance() {
    enum Input<'a> {
        Init,
        Operator(&'a str),
        Repository(&'a str),
        IsolatedRepository(&'a str),
    }

    enum Expectation {
        /// validate and compose must reach the same verdict, and it must be
        /// this one. Carrying the verdict as data keeps each case's expectation
        /// beside the case; deriving it from the case name instead would mean a
        /// rename silently changes what is asserted.
        Agree { accepted: bool },
    }

    // Declare the operation locally so isolation leaves exactly the actor
    // unresolved. Both context rows use these identical bytes.
    let repository_with_grants = format!(
        "{}operations: {{work: {{steps: []}}}}\n",
        Fixture::repository_policy("delegated")
    );
    let operator_with_grants =
        format!("{OPERATOR}grants:\n  delegated: {{actors: builder, operations: work}}\n");
    let cases = [
        (
            "unedited init output",
            Input::Init,
            Expectation::Agree { accepted: true },
        ),
        (
            "operator with grants",
            Input::Operator(&operator_with_grants),
            Expectation::Agree { accepted: true },
        ),
        (
            "repository without grants",
            Input::Repository("manifest_version: 1\n"),
            Expectation::Agree { accepted: true },
        ),
        (
            "grant with an actor absent from both scopes",
            Input::Repository(
                "manifest_version: 1\ngrants:\n  invalid: {actors: absent, operations: work}\n",
            ),
            // Resolvable in no scope, so both refuse even with a context.
            Expectation::Agree { accepted: false },
        ),
        (
            "repository grant naming an operator actor",
            Input::Repository(&repository_with_grants),
            Expectation::Agree { accepted: true },
        ),
        (
            "repository grant naming an operator actor in isolation",
            Input::IsolatedRepository(&repository_with_grants),
            // Strict acceptance is the definition: unresolved here is a refusal,
            // even though the default validate exits 0 and says so.
            Expectation::Agree { accepted: false },
        ),
    ];

    let mut saw_mutual_acceptance = false;
    let mut saw_mutual_rejection = false;
    for (name, input, expectation) in cases {
        let mut fixture = Fixture::new();
        if matches!(input, Input::Init | Input::Operator(_)) {
            fixture.manifest = fixture.home.join("ostrom.yaml");
        }
        match input {
            Input::Init => {
                fs::remove_file(&fixture.manifest).expect("remove fixture operator manifest");
                let init = fixture
                    .command()
                    .arg("init")
                    .output()
                    .expect("run ostrom init");
                assert!(
                    init.status.success(),
                    "{name}: {}",
                    String::from_utf8_lossy(&init.stderr)
                );
            }
            Input::IsolatedRepository(source) => {
                fs::remove_file(fixture.home.join("ostrom.yaml")).expect("remove operator context");
                fs::write(&fixture.manifest, source).expect("write isolated agreement case");
            }
            Input::Operator(source) | Input::Repository(source) => {
                fs::write(&fixture.manifest, source).expect("write agreement case");
            }
        }
        fixture.trusted_keys = support::sign_manifest(&fixture.manifest);

        // Strict exit status defines acceptance. Default validation may succeed
        // while explicitly reporting references unresolved in this same input.
        if matches!(input, Input::IsolatedRepository(_)) {
            let default = fixture
                .command()
                .arg("validate")
                .arg(&fixture.manifest)
                .output()
                .expect("default isolated validation");
            assert_eq!(
                default.status.code(),
                Some(0),
                "{}",
                String::from_utf8_lossy(&default.stderr)
            );
            let stdout = String::from_utf8(default.stdout).expect("UTF-8 diagnostics");
            assert_eq!(
                stdout.lines().next(),
                Some(
                    format!(
                        "valid: {} (isolated; 1 unresolved)",
                        fixture.manifest.display()
                    )
                    .as_str()
                )
            );
            assert!(
                stdout
                    .lines()
                    .any(|line| line == "unresolved: grants.delegated.actors -> builder"),
                "{stdout}"
            );
        }
        // Both commands receive the same signed file and operator context.
        let validate = fixture
            .command()
            .args(["validate", "--strict"])
            .arg(&fixture.manifest)
            .output()
            .expect("run ostrom validate");
        let compose = fixture.compose();
        let validate_accepts = validate.status.success();
        let compose_accepts = compose.status.success();
        let diagnostic = format!(
            "case `{name}`\nvalidate: accepted={validate_accepts}\nstdout: {}\nstderr: {}\ncompose: accepted={compose_accepts}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&validate.stdout).trim(),
            String::from_utf8_lossy(&validate.stderr).trim(),
            String::from_utf8_lossy(&compose.stdout).trim(),
            String::from_utf8_lossy(&compose.stderr).trim(),
        );
        let Expectation::Agree {
            accepted: expected_acceptance,
        } = expectation;
        assert_eq!(
            (validate_accepts, compose_accepts),
            (expected_acceptance, expected_acceptance),
            "{diagnostic}"
        );
        if !expected_acceptance {
            assert_eq!(validate.status.code(), Some(1), "{diagnostic}");
            assert_eq!(compose.status.code(), Some(1), "{diagnostic}");
        }
        // Every repository input carries an operator context, so an accepted
        // one must say it resolved against it. Keyed on the input rather than
        // the case name, so a rename cannot silently skip the assertion.
        if matches!(input, Input::Repository(_)) && expected_acceptance {
            assert_eq!(
                String::from_utf8_lossy(&validate.stdout).lines().next(),
                Some(
                    format!(
                        "valid: {} (resolved against operator {})",
                        fixture.manifest.display(),
                        fixture.home.join("ostrom.yaml").display()
                    )
                    .as_str()
                ),
                "{diagnostic}"
            );
        }
        saw_mutual_acceptance |= validate_accepts;
        saw_mutual_rejection |= !validate_accepts;
    }
    assert!(
        saw_mutual_acceptance,
        "cases must exercise mutual acceptance"
    );
    assert!(saw_mutual_rejection, "cases must exercise mutual rejection");
}

#[test]
fn validate_reports_bare_isolation() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.home.join("ostrom.yaml")).expect("remove context");
    fs::write(&fixture.manifest, "manifest_version: 1\n").expect("empty manifest");
    support::sign_manifest(&fixture.manifest);
    for strict in [false, true] {
        let mut command = fixture.command();
        command.arg("validate").arg(&fixture.manifest);
        if strict {
            command.arg("--strict");
        }
        let output = command.output().expect("validate isolated manifest");
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).expect("UTF-8 output"),
            format!("valid: {} (isolated)\n", fixture.manifest.display())
        );
    }
}

#[test]
fn validate_lists_all_unresolved_references_in_manifest_order_and_strict_refuses() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.home.join("ostrom.yaml")).expect("remove context");
    // Deliberately differ from sorted map, section, and validation field order.
    fs::write(&fixture.manifest, concat!(
        "manifest_version: 1\n",
        "denies:\n  z-last: {operations: [z-op, a-op], actors: [z-actor, a-actor]}\n",
        "loops:\n  tick: {operation: tick-op, actor: ticker, target: example/repo, every: hourly}\n",
        "grants:\n  z-last: {operations: work, actors: builder}\n  a-first: {actors: reviewer}\n",
        "includes: [z-leaf.yaml, a-fragment.yaml]\n",
    )).expect("write references");
    fs::write(
        fixture.repository.join("z-leaf.yaml"),
        "deny: leaf\noperations: leaf-op\nactors: leaf-actor\n",
    )
    .expect("write leaf");
    fs::write(
        fixture.repository.join("a-fragment.yaml"),
        "denies:\n  fragment: {actors: fragment-actor, operations: fragment-op}\n",
    )
    .expect("write fragment");
    support::sign_manifest(&fixture.manifest);
    let expected = format!(
        concat!(
            "valid: {} (isolated; 13 unresolved)\n",
            "unresolved: denies.z-last.operations -> z-op\n",
            "unresolved: denies.z-last.operations -> a-op\n",
            "unresolved: denies.z-last.actors -> z-actor\n",
            "unresolved: denies.z-last.actors -> a-actor\n",
            "unresolved: loops.tick.operation -> tick-op\n",
            "unresolved: loops.tick.actor -> ticker\n",
            "unresolved: grants.z-last.operations -> work\n",
            "unresolved: grants.z-last.actors -> builder\n",
            "unresolved: grants.a-first.actors -> reviewer\n",
            "unresolved: denies.leaf.operations -> leaf-op\n",
            "unresolved: denies.leaf.actors -> leaf-actor\n",
            "unresolved: denies.fragment.actors -> fragment-actor\n",
            "unresolved: denies.fragment.operations -> fragment-op\n",
        ),
        fixture.manifest.display()
    );
    for strict in [false, true] {
        let mut command = fixture.command();
        command.arg("validate").arg(&fixture.manifest);
        if strict {
            command.arg("--strict");
        }
        let output = command.output().expect("validate references");
        assert_eq!(
            output.status.code(),
            Some(i32::from(strict)),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).expect("UTF-8 output"),
            expected
        );
        if strict {
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("invalid policy manifest: 13 unresolved reference(s) in isolation")
            );
        }
    }
    let normalized = fixture
        .command()
        .args(["validate", "--normalized"])
        .arg(&fixture.manifest)
        .output()
        .expect("normalize unresolved manifest");
    assert_eq!(normalized.status.code(), Some(0));
    // --normalized keeps stdout a pure YAML document; the diagnostics go to
    // stderr so a consumer can pipe stdout straight into a parser.
    assert_eq!(
        String::from_utf8(normalized.stderr).expect("UTF-8 diagnostics"),
        expected
    );
    let yaml = String::from_utf8(normalized.stdout).expect("UTF-8 normalized output");
    let parsed = PolicyManifest::parse_yaml(&yaml).expect("normalized manifest");
    assert_eq!(
        parsed
            .validate_in_context(None)
            .expect("still unresolved")
            .len(),
        13
    );
}

#[test]
fn unresolved_order_preserves_yaml_keys_decoded_as_strings() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.home.join("ostrom.yaml")).expect("remove context");
    fs::write(
        &fixture.manifest,
        concat!(
            "manifest_version: 1\ngrants:\n",
            "  z: {actors: first}\n",
            "  12: {actors: second}\n",
            "  0xC: {actors: third}\n",
            "  true: {actors: fourth}\n",
            "  a: {actors: fifth}\n",
        ),
    )
    .expect("numeric and boolean rule names");
    support::sign_manifest(&fixture.manifest);
    let output = fixture
        .command()
        .arg("validate")
        .arg(&fixture.manifest)
        .output()
        .expect("validate string-decoded keys");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 output"),
        format!(
            concat!(
                "valid: {} (isolated; 5 unresolved)\n",
                "unresolved: grants.z.actors -> first\n",
                "unresolved: grants.12.actors -> second\n",
                "unresolved: grants.0xC.actors -> third\n",
                "unresolved: grants.true.actors -> fourth\n",
                "unresolved: grants.a.actors -> fifth\n",
            ),
            fixture.manifest.display()
        )
    );
}

#[test]
fn explicit_operator_overrides_discovery_and_resolves_the_named_file() {
    let fixture = Fixture::new();
    let operator = fixture.repository.join("chosen-policy.yaml");
    fs::write(&operator, OPERATOR).expect("explicit operator");
    support::sign_manifest(&operator);
    // Ambiguous discovery must not prevent an explicit context from being used.
    fs::write(fixture.home.join("ostrom.yml"), OPERATOR).expect("ambiguous discovery");
    let discovered = fixture
        .command()
        .arg("validate")
        .arg(&fixture.manifest)
        .output()
        .expect("discover ambiguous context");
    assert_eq!(discovered.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&discovered.stderr).contains("both policy manifest paths exist")
    );
    for strict in [false, true] {
        let mut command = fixture.command();
        command
            .arg("validate")
            .arg(&fixture.manifest)
            .arg("--operator")
            .arg(&operator);
        if strict {
            command.arg("--strict");
        }
        let output = command.output().expect("validate in explicit context");
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).expect("UTF-8 output"),
            format!(
                "valid: {} (resolved against operator {})\n",
                fixture.manifest.display(),
                operator.display()
            )
        );
    }
    fs::write(&operator, "manifest_version: 1\n").expect("tamper with explicit operator");
    let tampered = fixture
        .command()
        .arg("validate")
        .arg(&fixture.manifest)
        .arg("--operator")
        .arg(&operator)
        .output()
        .expect("verify explicit operator signature");
    assert_eq!(tampered.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&tampered.stderr).contains("signature"));

    for contents in [
        Some("manifest_version: 1\n"),
        Some("manifest_version: 2\n"),
        None,
    ] {
        if let Some(contents) = contents {
            fs::write(&operator, contents).expect("replace operator");
            support::sign_manifest(&operator);
        } else {
            fs::remove_file(&operator).expect("remove explicit context");
        }
        let output = fixture
            .command()
            .arg("validate")
            .arg(&fixture.manifest)
            .arg("--operator")
            .arg(&operator)
            .output()
            .expect("refuse invalid explicit context");
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        let expected = match contents {
            Some("manifest_version: 1\n") => "unknown actor `builder`",
            Some(_) => "manifest_version 2; expected 1",
            None => "could not read",
        };
        assert!(stderr.contains(expected), "{stderr}");
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn strict_isolated_acceptance_checks_effective_operation_scope() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.home.join("ostrom.yaml")).expect("remove context");
    fs::write(
        &fixture.manifest,
        format!("{OPERATOR}grants: {{delegated: {{actors: builder, operations: work}}}}\n"),
    )
    .expect("self-contained source");
    support::sign_manifest(&fixture.manifest);
    let default = fixture
        .command()
        .arg("validate")
        .arg(&fixture.manifest)
        .output()
        .expect("validate authored declarations");
    assert_eq!(
        default.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&default.stderr)
    );
    // The file declares actors and operations itself, so the isolation
    // assumption is load-bearing even though nothing is unresolved: the
    // default context line says so.
    assert_eq!(
        String::from_utf8(default.stdout).expect("UTF-8 output"),
        format!(
            "valid: {} (isolated; evaluated as repository policy)\n",
            fixture.manifest.display()
        )
    );
    // Repository operations are not adopted, even when their declarations are
    // well formed. Strict acceptance must check composition's effective scope.
    // The refusal must name the assumption it was evaluated under, not just
    // state the symptom (ostrom CLAUDE.md principle 5).
    for args in [vec!["validate", "--strict"], vec!["compose"]] {
        let output = fixture
            .command()
            .args(args)
            .arg(&fixture.manifest)
            .output()
            .expect("check effective acceptance");
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("unknown operation `work`"), "{stderr}");
        assert!(
            stderr.contains("as repository policy: "),
            "refusal must name the assumption it evaluated under: {stderr}"
        );
        assert!(
            stderr.find("as repository policy: ").unwrap()
                < stderr.find("unknown operation `work`").unwrap(),
            "the assumption must prefix the reason, not follow it: {stderr}"
        );
    }
}

#[test]
fn default_isolation_context_names_the_assumption_only_when_load_bearing() {
    // A bare manifest declares neither actors nor operations, so the
    // repository-policy assumption changes nothing about its verdict: the
    // plain `(isolated)` line stays, with no added words.
    let bare = Fixture::new();
    fs::remove_file(bare.home.join("ostrom.yaml")).expect("remove context");
    fs::write(&bare.manifest, "manifest_version: 1\n").expect("bare manifest");
    support::sign_manifest(&bare.manifest);
    let bare_output = bare
        .command()
        .arg("validate")
        .arg(&bare.manifest)
        .output()
        .expect("validate bare manifest");
    assert_eq!(bare_output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(bare_output.stdout).expect("UTF-8 output"),
        format!("valid: {} (isolated)\n", bare.manifest.display())
    );

    // A manifest that declares actors or operations is judged as repository
    // policy the moment there is no operator to adopt it, even when every
    // reference in it resolves. Say so.
    let declaring = Fixture::new();
    fs::remove_file(declaring.home.join("ostrom.yaml")).expect("remove context");
    fs::write(&declaring.manifest, OPERATOR).expect("self-contained declarations");
    support::sign_manifest(&declaring.manifest);
    let declaring_output = declaring
        .command()
        .arg("validate")
        .arg(&declaring.manifest)
        .output()
        .expect("validate declaring manifest");
    assert_eq!(declaring_output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(declaring_output.stdout).expect("UTF-8 output"),
        format!(
            "valid: {} (isolated; evaluated as repository policy)\n",
            declaring.manifest.display()
        )
    );
}

#[test]
fn strict_help_defines_acceptance_by_exit_status() {
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["validate", "--help"])
        .output()
        .expect("validate help");
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(help.contains("--strict"), "{help}");
    assert!(
        help.contains("Define acceptance by exit status: unresolved references are invalid"),
        "{help}"
    );
}

#[test]
fn strict_validation_and_composition_share_remaining_acceptance_checks() {
    let fixture = Fixture::new();
    for (source, message) in [
        (
            "manifest_version: 1\ninputs: {count: {type: integer, env: OSTROM_TEST_466_COUNT}}\n",
            "input `count`",
        ),
        (
            "manifest_version: 1\ngrants: {bad: {where: 'actor:absent'}}\n",
            "invalid selector",
        ),
    ] {
        fs::write(&fixture.manifest, source).expect("write invalid policy");
        support::sign_manifest(&fixture.manifest);
        for args in [
            vec!["validate", "--strict"],
            vec!["compose"],
            vec!["validate", "--strict", "--unsigned"],
            vec!["compose", "--unsigned"],
        ] {
            let output = fixture
                .command()
                .env("OSTROM_TEST_466_COUNT", "not-an-integer")
                .args(args)
                .arg(&fixture.manifest)
                .output()
                .expect("check acceptance");
            assert_eq!(output.status.code(), Some(1));
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(message),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
fn init_manifest_validates_and_composes_as_operator_policy() {
    let root = TempDir::new().expect("temporary init fixture");
    let home = root.path().join("home");
    fs::create_dir(&home).expect("create operator home");

    let init = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .env("OSTROM_HOME", &home)
        .arg("init")
        .output()
        .expect("run ostrom init");
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );

    let manifest = home.join("ostrom.yaml");
    let trusted_keys = support::sign_manifest(&manifest);
    let validate = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .env("OSTROM_HOME", &home)
        .env("OSTROM_POLICY_TRUSTED_KEYS", &trusted_keys)
        .args(["validate"])
        .arg(&manifest)
        .output()
        .expect("validate init manifest");
    assert!(
        validate.status.success(),
        "validate rejected the init manifest: {}",
        String::from_utf8_lossy(&validate.stderr)
    );

    let compose = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .current_dir(&home)
        .env("OSTROM_HOME", &home)
        .env("OSTROM_POLICY_TRUSTED_KEYS", &trusted_keys)
        .arg("compose")
        .output()
        .expect("compose init manifest");
    assert!(
        compose.status.success(),
        "compose disagreed with validate: {}",
        String::from_utf8_lossy(&compose.stderr)
    );

    let current = fs::read_link(home.join("current")).expect("read current policy pointer");
    let composed = fs::read_to_string(home.join(current).join("ostrom.yaml"))
        .expect("read composed operator policy");
    let composed = PolicyManifest::from_yaml(&composed).expect("parse composed operator policy");
    assert!(composed.operations.contains_key("build-pass"));
    assert!(composed.operations.contains_key("gate-pass"));
    assert!(
        composed
            .grants
            .get("builder-build")
            .expect("builder grant retained")
            .operations
            .iter()
            .any(|operation| operation == "build-pass")
    );
    assert!(
        composed
            .grants
            .get("gatekeeper-gate")
            .expect("gatekeeper grant retained")
            .operations
            .iter()
            .any(|operation| operation == "gate-pass")
    );
}
