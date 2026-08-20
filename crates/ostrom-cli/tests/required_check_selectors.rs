use std::{fs, process::Command};

use tempfile::TempDir;

struct Fixture {
    root: TempDir,
}

impl Fixture {
    fn new(workflow: &str, selectors: Option<&[&str]>) -> Self {
        let root = tempfile::tempdir().expect("required check selector fixture");
        let workflows = root.path().join("repository/.github/workflows");
        let home = root.path().join("ostrom-home");
        fs::create_dir_all(&workflows).expect("workflow directory");
        fs::create_dir_all(&home).expect("Ostrom home");
        fs::write(workflows.join("test.yml"), workflow).expect("workflow fixture");
        if let Some(selectors) = selectors {
            let selectors = selectors
                .iter()
                .map(|selector| format!("      - {selector}"))
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(
                home.join("gate.yaml"),
                format!(
                    "provider: file\nbounce_all: []\nprojects:\n  - repo: placeholder-org/alpha\n    required_checks:\n{selectors}\n    bounce: []\n    reserved: []\n"
                ),
            )
            .expect("gate fixture");
        }
        Self { root }
    }

    fn run(&self) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_ostrom"))
            .args(["check", "required-check-selectors"])
            .env("HOME", self.root.path())
            .env("OSTROM_HOME", self.root.path().join("ostrom-home"))
            .env("GITHUB_REPOSITORY", "placeholder-org/alpha")
            .current_dir(self.root.path().join("repository"))
            .output()
            .expect("run required check selector check")
    }
}

#[test]
fn subcommand_passes_when_all_selectors_match() {
    let fixture = Fixture::new(
        "jobs:\n  rust:\n    runs-on: ubuntu-latest\n    steps: []\n",
        Some(&["rust"]),
    );

    let output = fixture.run();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn subcommand_fails_when_a_selector_is_dead() {
    let fixture = Fixture::new(
        "jobs:\n  rust:\n    runs-on: ubuntu-latest\n    steps: []\n",
        Some(&["rust", "removed-tools"]),
    );

    let output = fixture.run();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("'removed-tools' matches no job name"));
}

#[test]
fn subcommand_clearly_skips_when_gate_configuration_is_absent() {
    let fixture = Fixture::new(
        "jobs:\n  rust:\n    runs-on: ubuntu-latest\n    steps: []\n",
        None,
    );

    let output = fixture.run();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(stdout.contains("required check selectors: skipped: no gate.yaml found"));
    assert!(output.stderr.is_empty());
}
