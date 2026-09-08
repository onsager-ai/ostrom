#![cfg(unix)]

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;

mod support;

const MARKER: &str =
    "composition performed without verifying the candidate's signature; nothing was written\n";
const TRUST_REQUIRED: &str = "OSTROM_POLICY_TRUSTED_KEYS is required to load a policy manifest\n";

struct Fixture {
    root: TempDir,
    home: PathBuf,
    manifest: PathBuf,
    trusted: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().expect("unsigned policy fixture");
        let home = root.path().join("home");
        fs::create_dir_all(home.join("state/empty")).expect("nested state directory");
        fs::write(home.join("state/receipt"), b"preserved\0bytes\n").expect("state receipt");
        fs::write(
            home.join("actor.yaml"),
            "actor: builder\npermission_mode: manual\n",
        )
        .expect("included actor");
        fs::write(home.join("prompt.md"), "Draft policy.\n").expect("prompt file");
        let manifest = home.join("ostrom.yaml");
        fs::write(
            &manifest,
            concat!(
                "manifest_version: 1\nincludes: [actor.yaml]\n",
                "prompts: {work: {from: prompt.md}}\n",
                "operations: {work: {steps: []}}\n",
            ),
        )
        .expect("operator policy");
        let trusted = support::sign_manifest(&manifest);
        Self {
            root,
            home,
            manifest,
            trusted,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        command
            .env_clear()
            .current_dir(&self.home)
            .env("OSTROM_HOME", &self.home)
            .env("OSTROM_POLICY_TRUSTED_KEYS", &self.trusted);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .arg(&self.manifest)
            .output()
            .expect("run ostrom")
    }

    fn remove_signature(&self) {
        fs::remove_file(self.manifest.with_extension("yaml.sig")).expect("remove signature");
    }

    fn change_candidate(&self) {
        fs::write(self.home.join("prompt.md"), "Revise draft policy.\n").expect("edit prompt");
        self.remove_signature();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(entries) = fs::read_dir(self.home.join("versions")) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o755))
                        .expect("unseal version for fixture cleanup");
                }
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Contents {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
}

type Tree = BTreeMap<PathBuf, (u32, Contents)>;

fn snapshot(root: &Path) -> Tree {
    fn visit(root: &Path, path: &Path, tree: &mut Tree) {
        let metadata = fs::symlink_metadata(path).expect("snapshot metadata");
        let contents = if metadata.is_symlink() {
            Contents::Symlink(fs::read_link(path).expect("snapshot symlink target"))
        } else if metadata.is_dir() {
            for entry in fs::read_dir(path).expect("snapshot directory") {
                visit(root, &entry.expect("snapshot entry").path(), tree);
            }
            Contents::Directory
        } else {
            assert!(
                metadata.is_file(),
                "unexpected file type: {}",
                path.display()
            );
            Contents::File(fs::read(path).expect("snapshot every file's bytes"))
        };
        tree.insert(
            path.strip_prefix(root).expect("relative path").to_owned(),
            (metadata.permissions().mode(), contents),
        );
    }
    let mut tree = Tree::new();
    visit(root, root, &mut tree);
    tree
}

fn assert_unchanged(before: &Tree, after: &Tree) {
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "OSTROM_HOME paths changed"
    );
    for (path, contents) in before {
        assert_eq!(
            Some(contents),
            after.get(path),
            "OSTROM_HOME entry changed: {}",
            path.display()
        );
    }
}

fn assert_success(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn digest(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout)
        .expect("compose stdout")
        .split_whitespace()
        .find_map(|word| word.strip_prefix("digest="))
        .expect("composed digest")
}

#[test]
fn signed_and_unsigned_compose_stdout_and_normalized_manifest_bytes_agree() {
    let fixture = Fixture::new();
    let signed = fixture.run(&["compose"]);
    assert_success(&signed);
    assert!(signed.stderr.is_empty());
    let composed = fs::read(
        fixture
            .home
            .join("versions")
            .join(digest(&signed))
            .join("ostrom.yaml"),
    )
    .expect("signed composed manifest");
    let normalized = fixture.run(&["validate", "--normalized"]);
    assert_success(&normalized);
    assert_eq!(normalized.stdout, composed);

    // Exercise a valid sidecar, a missing sidecar, and a malformed sidecar.
    for signature in [Some("valid"), None, Some("malformed")] {
        match signature {
            Some("valid") => {}
            None => fixture.remove_signature(),
            Some(_) => fs::write(fixture.manifest.with_extension("yaml.sig"), "malformed")
                .expect("malformed signature"),
        }
        let before = snapshot(&fixture.home);
        let unsigned = fixture.run(&["compose", "--unsigned"]);
        assert_success(&unsigned);
        assert_eq!(signed.stdout, unsigned.stdout);
        assert_eq!(digest(&signed), digest(&unsigned));
        assert_eq!(unsigned.stderr, MARKER.as_bytes());
        assert_ne!(signed.stderr, unsigned.stderr);
        let unsigned_normalized = fixture.run(&["validate", "--unsigned", "--normalized"]);
        assert_success(&unsigned_normalized);
        assert_eq!(unsigned_normalized.stdout, composed);
        assert_eq!(
            unsigned_normalized.stderr,
            [normalized.stderr.as_slice(), MARKER.as_bytes()].concat()
        );
        assert_unchanged(&before, &snapshot(&fixture.home));
    }
}

#[test]
fn unsigned_compose_leaves_the_whole_home_tree_unchanged() {
    for with_current in [false, true] {
        let fixture = Fixture::new();
        if with_current {
            assert_success(&fixture.run(&["compose"]));
        }
        fixture.change_candidate();
        let before = snapshot(&fixture.home);
        let output = fixture.run(&["compose", "--unsigned"]);
        assert_success(&output);
        assert_unchanged(&before, &snapshot(&fixture.home));
        assert!(!fixture.home.join("versions").join(digest(&output)).exists());
    }
}

#[test]
fn unsigned_validate_leaves_the_whole_home_tree_unchanged() {
    let fixture = Fixture::new();
    assert_success(&fixture.run(&["compose"]));
    fixture.change_candidate();
    for args in [
        vec!["validate", "--unsigned"],
        vec!["validate", "--unsigned", "--strict"],
        vec!["validate", "--unsigned", "--normalized"],
    ] {
        let before = snapshot(&fixture.home);
        assert_success(&fixture.run(&args));
        assert_unchanged(&before, &snapshot(&fixture.home));
    }
}

#[test]
fn missing_signature_requires_the_explicit_flag_on_both_commands() {
    let fixture = Fixture::new();
    fixture.remove_signature();
    for command in ["compose", "validate"] {
        let refused = fixture.run(&[command]);
        assert_eq!(refused.status.code(), Some(1));
        assert!(refused.stdout.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&refused.stderr),
            format!(
                "policy signature is missing: `{}`\n",
                fixture.manifest.with_extension("yaml.sig").display()
            )
        );
        let accepted = fixture.run(&[command, "--unsigned"]);
        assert_success(&accepted);
        assert_eq!(accepted.stderr, MARKER.as_bytes());
    }
}

#[test]
fn environment_cannot_enable_unsigned_and_missing_trust_still_refuses() {
    let fixture = Fixture::new();
    for has_signature in [true, false] {
        if !has_signature {
            fixture.remove_signature();
        }
        for command in ["compose", "validate"] {
            for trust in [
                Some(fixture.trusted.as_os_str()),
                None,
                Some(std::ffi::OsStr::new("")),
            ] {
                let mut process = fixture.command();
                process.args([command]).arg(&fixture.manifest).envs([
                    ("OSTROM_UNSIGNED", "true"),
                    ("OSTROM_POLICY_UNSIGNED", "true"),
                    ("OSTROM_COMPOSE_UNSIGNED", "true"),
                    ("OSTROM_VALIDATE_UNSIGNED", "true"),
                ]);
                if let Some(trust) = trust {
                    process.env("OSTROM_POLICY_TRUSTED_KEYS", trust);
                } else {
                    process.env_remove("OSTROM_POLICY_TRUSTED_KEYS");
                }
                let output = process
                    .output()
                    .expect("probe environment without argv flag");
                if has_signature && trust.is_some_and(|path| !path.is_empty()) {
                    assert_success(&output);
                    assert!(output.stderr.is_empty());
                } else {
                    assert_eq!(output.status.code(), Some(1));
                    assert!(output.stdout.is_empty());
                    if trust.is_none_or(|path| path.is_empty()) {
                        assert_eq!(output.stderr, TRUST_REQUIRED.as_bytes());
                    } else {
                        assert!(
                            String::from_utf8_lossy(&output.stderr)
                                .contains("policy signature is missing")
                        );
                    }
                }
            }
            for trust in [None, Some("")] {
                let mut process = fixture.command();
                process.args([command, "--unsigned"]).arg(&fixture.manifest);
                if let Some(trust) = trust {
                    process.env("OSTROM_POLICY_TRUSTED_KEYS", trust);
                } else {
                    process.env_remove("OSTROM_POLICY_TRUSTED_KEYS");
                }
                assert_success(
                    &process
                        .output()
                        .expect("explicit flag needs no trusted keys"),
                );
            }
        }
    }
}

#[test]
fn generate_still_requires_a_signature_and_rejects_the_unsigned_flag() {
    let fixture = Fixture::new();
    fixture.remove_signature();
    let before = snapshot(&fixture.home);
    let refused = fixture
        .command()
        .args(["generate", "example/repository"])
        .env("OSTROM_UNSIGNED", "true")
        .output()
        .expect("generate unsigned operator policy");
    assert_eq!(refused.status.code(), Some(1));
    assert!(refused.stdout.is_empty());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("policy signature is missing"));
    let unsupported = fixture
        .command()
        .args(["generate", "--unsigned", "example/repository"])
        .output()
        .expect("generate has no unsigned flag");
    assert_eq!(unsupported.status.code(), Some(2));
    assert_unchanged(&before, &snapshot(&fixture.home));
}

#[test]
fn unsigned_validation_preserves_resolution_context_and_strict_refusal() {
    let fixture = Fixture::new();
    let candidate = fixture.root.path().join("repository.yaml");
    fs::write(
        &candidate,
        concat!(
            "manifest_version: 1\noperations: {work: {steps: []}}\n",
            "grants: {delegated: {actors: builder, operations: work}}\n",
        ),
    )
    .expect("repository candidate");
    for explicit in [false, true] {
        let mut command = fixture.command();
        command
            .args(["validate", "--unsigned", "--strict"])
            .arg(&candidate);
        if explicit {
            command.arg("--operator").arg(&fixture.manifest);
        }
        let output = command.output().expect("validate in operator context");
        assert_success(&output);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!(
                "valid: {} (resolved against operator {})\n",
                candidate.display(),
                fixture.manifest.display()
            )
        );
        assert_eq!(output.stderr, MARKER.as_bytes());
    }
    fixture.remove_signature();
    for command in ["compose", "validate"] {
        let output = fixture
            .command()
            .args([command, "--unsigned"])
            .arg(&candidate)
            .output()
            .expect("operator context still requires a signature");
        assert_eq!(output.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&output.stderr).contains("policy signature is missing"));
    }
    fs::remove_file(&fixture.manifest).expect("remove operator context");
    for strict in [false, true] {
        let before = snapshot(&fixture.home);
        let mut command = fixture.command();
        command.args(["validate", "--unsigned"]).arg(&candidate);
        if strict {
            command.arg("--strict");
        }
        let output = command.output().expect("validate in isolation");
        assert_eq!(output.status.code(), Some(i32::from(strict)));
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!(
                "valid: {} (isolated; 1 unresolved)\nunresolved: grants.delegated.actors -> builder\n",
                candidate.display()
            )
        );
        let expected = if strict {
            format!("{MARKER}invalid policy manifest: 1 unresolved reference(s) in isolation\n")
        } else {
            MARKER.to_owned()
        };
        assert_eq!(output.stderr, expected.as_bytes());
        assert_unchanged(&before, &snapshot(&fixture.home));
    }
    fs::write(&candidate, "manifest_version: 1\n").expect("self-contained isolated policy");
    let output = fixture
        .command()
        .args(["validate", "--unsigned", "--strict"])
        .arg(&candidate)
        .output()
        .expect("validate bare isolation");
    assert_success(&output);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("valid: {} (isolated)\n", candidate.display())
    );
}
