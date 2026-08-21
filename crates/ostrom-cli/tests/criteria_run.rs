#![cfg(unix)]

use std::{fs, path::PathBuf, process::Command, time::Instant};

use ostrom_checks::ActionRegistry;
use ostrom_core::{
    Catalogue, CatalogueEnumeration, CheckDocument, CheckRun, CheckVerdict, GateConfig,
    PolicyManifest,
};
use tempfile::tempdir;

#[test]
fn check_run_records_verdicts_and_isolates_a_timeout() {
    let home = tempdir().expect("criteria home");
    let continued = home.path().join("continued");
    let continue_script = format!("printf continued > {}", continued.display());
    fs::write(
        home.path().join("checks.yaml"),
        format!(
            concat!(
                "checks_version: 1\n",
                "checks:\n",
                "  a-pass:\n",
                "    uses: cmd/run\n",
                "    with: {{script: {}}}\n",
                "  b-fail:\n",
                "    uses: cmd/run\n",
                "    with: {{script: {}}}\n",
                "  c-timeout:\n",
                "    uses: cmd/run\n",
                "    with: {{script: {}, timeout: 10ms}}\n",
                "  z-after-timeout:\n",
                "    uses: cmd/run\n",
                "    with: {{script: {}}}\n",
            ),
            serde_json::to_string("exit 0").expect("quote pass"),
            serde_json::to_string("exit 1").expect("quote fail"),
            serde_json::to_string("sleep 1").expect("quote timeout"),
            serde_json::to_string(&continue_script).expect("quote continuation"),
        ),
    )
    .expect("write checks");

    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["check", "run"])
        .env("OSTROM_HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("execute criteria");

    assert!(!output.status.success(), "a failing verdict fails the pass");
    assert!(
        started.elapsed().as_secs() < 1,
        "the timed-out criterion stalled the pass"
    );
    assert!(
        continued.exists(),
        "criteria after a timeout must still run"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("ostrom check run: 2 passed; 1 failed; 1 inconclusive; 0 faulted")
    );

    let journal = fs::read_to_string(home.path().join("check-runs.jsonl"))
        .expect("read isolated check journal");
    let lines = journal.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1);
    let run: CheckRun = serde_json::from_str(lines[0]).expect("decode check run");
    assert_eq!(run.receipts.len(), 4);
    let receipt = |id: &str| {
        run.receipts
            .iter()
            .find(|receipt| receipt.check == id)
            .expect("criterion receipt")
    };
    assert_eq!(receipt("a-pass").verdict, Some(CheckVerdict::Pass));
    assert_eq!(receipt("b-fail").verdict, Some(CheckVerdict::Fail));
    assert_eq!(receipt("b-fail").error, None);
    assert_eq!(
        receipt("c-timeout").verdict,
        Some(CheckVerdict::Inconclusive)
    );
    assert_eq!(receipt("c-timeout").error, None);
    assert_eq!(receipt("z-after-timeout").verdict, Some(CheckVerdict::Pass));
}

#[test]
fn pass_policy_allows_an_inconclusive_run_and_emits_a_warning() {
    let home = tempdir().expect("criteria home");
    fs::write(
        home.path().join("checks.yaml"),
        concat!(
            "checks_version: 1\n",
            "inconclusive_policy: block\n",
            "checks:\n",
            "  unavailable:\n",
            "    uses: cmd/run\n",
            "    inconclusive_policy: pass\n",
            "    with: {script: \"sleep 1\", timeout: 10ms}\n",
        ),
    )
    .expect("write checks");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["check", "run"])
        .env("OSTROM_HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("execute criteria");

    assert!(
        output.status.success(),
        "pass policy blocked the run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains(
        "warning: check unavailable was inconclusive and allowed by inconclusive_policy: pass"
    ));
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("0 passed; 0 failed; 1 inconclusive; 0 faulted")
    );
}

#[test]
fn check_run_is_a_reachable_successful_cli_surface() {
    let home = tempdir().expect("criteria home");
    fs::write(
        home.path().join("checks.yaml"),
        "checks_version: 1\nchecks:\n  ready:\n    uses: cmd/run\n    with: {script: \"exit 0\"}\n",
    )
    .expect("write checks");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["check", "run"])
        .env("OSTROM_HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("execute criteria");
    assert!(
        output.status.success(),
        "check run stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(home.path().join("check-runs.jsonl").is_file());
}

#[test]
fn unknown_action_fails_the_catalogue_load_and_writes_no_run() {
    let home = tempdir().expect("criteria home");
    fs::write(
        home.path().join("checks.yaml"),
        "checks_version: 1\nchecks:\n  unknown:\n    uses: missing/observe\n    with: {}\n",
    )
    .expect("write checks");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["check", "run"])
        .env("OSTROM_HOME", home.path())
        .current_dir(home.path())
        .output()
        .expect("execute criteria");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown_action"));
    assert!(!home.path().join("check-runs.jsonl").exists());
}

#[test]
fn ten_legacy_gate_strings_map_to_valid_named_check_fixtures() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/checks-verdict");
    let gate = GateConfig::from_yaml(
        &fs::read_to_string(fixture.join("gate.before.yaml")).expect("legacy gate fixture"),
    )
    .expect("legacy gate parses");
    let document = CheckDocument::from_yaml(
        &fs::read_to_string(fixture.join("checks.yaml")).expect("checks fixture"),
    )
    .expect("checks fixture parses");
    let manifest = PolicyManifest::from_yaml(
        &fs::read_to_string(fixture.join("manifest.yml")).expect("manifest fixture"),
    )
    .expect("manifest fixture parses");
    assert_eq!(gate.projects.len(), 10);
    assert_eq!(document.checks.len(), 10);

    let requirements = manifest.operations["merge-placeholder-repositories"]
        .steps
        .iter()
        .map(|step| step.requires.as_deref().expect("step cites a check"))
        .collect::<Vec<_>>();
    assert_eq!(requirements.len(), 10);

    let enumeration = CatalogueEnumeration {
        catalogues: vec![Catalogue { document }],
        complete: true,
    };
    let registry = ActionRegistry::core(&fixture, &fixture).expect("core registry");
    for (index, project) in gate.projects.iter().enumerate() {
        let legacy_name = project
            .required_checks
            .first()
            .expect("one legacy required check");
        let check_id = format!("placeholder-ci-{:02}", index + 1);
        assert_eq!(requirements[index], check_id);
        let definition = &enumeration.catalogues[0].document.checks[&check_id];
        assert_eq!(definition.with["name"], legacy_name.as_str());
        registry
            .prepare(&check_id, &enumeration)
            .unwrap_or_else(|error| panic!("{check_id} did not validate: {error}"));
    }
}
