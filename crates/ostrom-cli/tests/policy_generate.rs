use std::{fs, path::Path, process::Command};

use ostrom_core::PolicyManifest;
use serde_json::json;
use tempfile::{TempDir, tempdir};

struct Fixture {
    home: TempDir,
    repository: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = tempdir().expect("temporary Ostrom home");
        let repository = home.path().join("repositories/placeholder-org/repository");
        fs::create_dir_all(repository.join(".git")).expect("fixture repository");
        let search_root = home.path().join("repositories");
        fs::write(
            home.path().join("mandates.yaml"),
            format!(
                r#"provider: file
cadence_hours: 1
stuck_after_days: 7
search_roots: [{}]
bounce_all: []
projects:
  - repo: placeholder-org/repository
    delegated: [label:delegated]
    excluded: [label:excluded]
    reserved: [199]
    default: unclassified
    paused: true
    # Releases are reviewed because publication is irreversible.
    bounce: [title:*release*]
"#,
                search_root.display()
            ),
        )
        .expect("central mandates");
        fs::write(
            home.path().join("gate.yaml"),
            r#"provider: file
bounce_all: []
projects:
  - repo: placeholder-org/repository
    required_checks: [ci-*]
    # Infrastructure changes need a human because they alter deployment.
    bounce: [path:infra/**]
    reserved: [178]
"#,
        )
        .expect("central gate");
        let records = json!({
            "repos": {
                "placeholder-org/repository": {
                    "records": {
                        "placeholder-org/repository#1": item(1, "issue", "fix: delegated", ["delegated"], [], []),
                        "placeholder-org/repository#2": item(2, "issue", "fix: excluded", ["excluded"], [], []),
                        "placeholder-org/repository#3": item(3, "issue", "release: public", [], [], []),
                        "placeholder-org/repository#4": item(4, "pr", "fix: infrastructure", [], ["infra/main.tf"], [("ci-rust", "SUCCESS")]),
                        "placeholder-org/repository#5": item(5, "issue", "fix: unmatched", [], [], []),
                        "placeholder-org/repository#6": item(6, "pr", "fix: delegated pr", ["delegated"], ["src/lib.rs"], [("ci-rust", "FAILURE")]),
                    }
                }
            }
        });
        fs::write(
            home.path().join("state.json"),
            serde_json::to_vec_pretty(&records).expect("state JSON"),
        )
        .expect("sweep state");
        Self { home, repository }
    }

    fn command(&self, arguments: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_ostrom"))
            .env("OSTROM_HOME", self.home.path())
            .args(arguments)
            .output()
            .expect("run ostrom")
    }

    fn manifest_path(&self) -> std::path::PathBuf {
        self.repository.join("ostrom.yaml")
    }
}

fn item<const L: usize, const F: usize, const C: usize>(
    number: u64,
    kind: &str,
    title: &str,
    labels: [&str; L],
    files: [&str; F],
    checks: [(&str, &str); C],
) -> serde_json::Value {
    json!({
        "id": format!("placeholder-org/repository#{number}"),
        "repo": "placeholder-org/repository",
        "ref": format!("#{number}"),
        "type": kind,
        "title": title,
        "labels": labels.to_vec(),
        "refs": [number],
        "files": files.to_vec(),
        "checks": checks.into_iter().map(|(name, state)| json!({"name": name, "state": state})).collect::<Vec<_>>(),
    })
}

fn load(path: &Path) -> (String, PolicyManifest) {
    let source = fs::read_to_string(path).expect("generated manifest source");
    let manifest = PolicyManifest::from_yaml(&source).expect("valid generated manifest");
    (source, manifest)
}

#[test]
fn generates_portable_equivalent_manifests_without_operator_state() {
    let fixture = Fixture::new();
    let generated = fixture.command(&["policy", "generate"]);
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    assert!(
        generated.stderr.is_empty(),
        "generated policy must pass lint"
    );
    let (source, manifest) = load(&fixture.manifest_path());
    assert!(manifest.actors.is_empty());
    assert!(
        manifest
            .grants
            .values()
            .chain(manifest.denies.values())
            .all(|rule| rule.actors.is_empty())
    );
    assert!(!source.contains("reserved"), "{source}");
    assert!(!source.contains("paused"), "{source}");
    assert!(!source.contains("builder"), "{source}");
    assert!(source.contains("delegated-changes"), "{source}");
    assert!(source.contains("excluded-changes"), "{source}");
    assert!(source.contains("operator-review"), "{source}");
    assert!(source.contains("ci-green"), "{source}");
    assert!(source.contains("description:"), "{source}");
    assert!(
        source.contains("Infrastructure changes need a human because they alter deployment."),
        "{source}"
    );
    assert!(
        source.contains("Releases are reviewed because publication is irreversible."),
        "{source}"
    );

    let verified = fixture.command(&["policy", "generate", "--verify"]);
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );
    assert!(
        String::from_utf8_lossy(&verified.stdout).contains("6 open items"),
        "{}",
        String::from_utf8_lossy(&verified.stdout)
    );
}

#[test]
fn verify_detects_a_seeded_divergence_and_names_the_item() {
    let fixture = Fixture::new();
    assert!(fixture.command(&["policy", "generate"]).status.success());
    let path = fixture.manifest_path();
    let source = fs::read_to_string(&path)
        .expect("generated source")
        .replace("label:delegated", "label:deliberately-wrong");
    fs::write(&path, source).expect("seed divergent manifest");

    let verified = fixture.command(&["policy", "generate", "--verify"]);
    assert!(!verified.status.success());
    let stderr = String::from_utf8(verified.stderr).expect("UTF-8 divergence");
    assert!(stderr.contains("diverges"), "{stderr}");
    assert!(stderr.contains("placeholder-org/repository#1"), "{stderr}");
    assert!(stderr.contains("central=granted"), "{stderr}");
    assert!(stderr.contains("generated=denied"), "{stderr}");
}
