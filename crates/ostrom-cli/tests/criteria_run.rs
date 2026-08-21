#![cfg(unix)]

use std::{fs, process::Command, time::Instant};

use ostrom_core::{CheckRun, CheckVerdict};
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
            serde_json::to_string("exit 7").expect("quote fail"),
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
