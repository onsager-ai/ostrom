use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;

mod support;

const VALID_GOALS: &str = concat!(
    "goals_version: 1\n",
    "goals:\n",
    "  - id: goal-a\n",
    "    intent: Ship reliable governance\n",
    "    state: active\n",
    "    serves: []\n",
    "    met_when: []\n",
    "actions: []\n",
    "acknowledgements: []\n",
);

const MALFORMED_YAML: &str = "goals_version: 1\ngoals: [\n";

const UNSUPPORTED_VERSION: &str = "goals_version: 2\n";

const DUPLICATE_GOAL: &str = concat!(
    "goals_version: 1\n",
    "goals:\n",
    "  - id: goal-a\n",
    "    intent: First\n",
    "    state: active\n",
    "  - id: goal-a\n",
    "    intent: Second\n",
    "    state: active\n",
    "actions: []\n",
    "acknowledgements: []\n",
);

const DUPLICATE_CHECK: &str = concat!(
    "goals_version: 1\n",
    "goals:\n",
    "  - id: goal-a\n",
    "    intent: First\n",
    "    state: active\n",
    "    met_when: [check-a, check-a]\n",
    "actions: []\n",
    "acknowledgements: []\n",
);

const UNKNOWN_GOAL_ACTION: &str = concat!(
    "goals_version: 1\n",
    "goals:\n",
    "  - id: goal-a\n",
    "    intent: First\n",
    "    state: active\n",
    "actions:\n",
    "  - goal: goal-b\n",
    "    verb: promote\n",
    "    note: bump priority\n",
    "acknowledgements: []\n",
);

fn ostrom(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
    command.env("OSTROM_HOME", home);
    command
}

fn validate(home: &Path, cwd: &Path, path: Option<&Path>) -> Output {
    let mut command = ostrom(home);
    command.args(["goals", "validate"]);
    if let Some(path) = path {
        command.arg(path);
    }
    command
        .current_dir(cwd)
        .output()
        .expect("run ostrom goals validate")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_valid_goals_document_exits_zero_and_prints_its_path() {
    let home = TempDir::new().expect("home");
    let goals = home.path().join("goals.yaml");
    fs::write(&goals, VALID_GOALS).expect("write goals");

    let output = validate(home.path(), home.path(), Some(&goals));

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("valid: {}\n", goals.display()));
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
}

#[test]
fn a_missing_document_refuses_with_code_66() {
    let home = TempDir::new().expect("home");
    let missing = home.path().join("missing-goals.yaml");

    let output = validate(home.path(), home.path(), Some(&missing));

    assert_eq!(output.status.code(), Some(66), "{}", stderr(&output));
    assert!(stdout(&output).is_empty());
    assert!(!stderr(&output).is_empty());
}

#[test]
fn malformed_yaml_refuses_with_code_3() {
    let home = TempDir::new().expect("home");
    let goals = home.path().join("goals.yaml");
    fs::write(&goals, MALFORMED_YAML).expect("write goals");

    let output = validate(home.path(), home.path(), Some(&goals));

    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert!(stdout(&output).is_empty());
}

#[test]
fn unsupported_goals_version_refuses_with_code_4() {
    let home = TempDir::new().expect("home");
    let goals = home.path().join("goals.yaml");
    fs::write(&goals, UNSUPPORTED_VERSION).expect("write goals");

    let output = validate(home.path(), home.path(), Some(&goals));

    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(stdout(&output).is_empty());
}

#[test]
fn semantically_invalid_documents_all_refuse_with_code_5() {
    let cases = [
        ("duplicate goal id", DUPLICATE_GOAL),
        ("repeated met_when check", DUPLICATE_CHECK),
        ("action naming an unknown goal", UNKNOWN_GOAL_ACTION),
    ];
    for (label, yaml) in cases {
        let home = TempDir::new().expect("home");
        let goals = home.path().join("goals.yaml");
        fs::write(&goals, yaml).expect("write goals");

        let output = validate(home.path(), home.path(), Some(&goals));

        assert_eq!(
            output.status.code(),
            Some(5),
            "{label}: {}",
            stderr(&output)
        );
        assert!(stdout(&output).is_empty(), "{label}");
    }
}

#[test]
fn the_four_refusal_codes_are_pairwise_distinct() {
    let home = TempDir::new().expect("home");
    let missing = home.path().join("missing.yaml");
    let malformed = home.path().join("malformed.yaml");
    fs::write(&malformed, MALFORMED_YAML).expect("write malformed goals");
    let unsupported = home.path().join("unsupported.yaml");
    fs::write(&unsupported, UNSUPPORTED_VERSION).expect("write unsupported-version goals");
    let invalid = home.path().join("invalid.yaml");
    fs::write(&invalid, DUPLICATE_GOAL).expect("write semantically invalid goals");

    let codes = [
        validate(home.path(), home.path(), Some(&missing))
            .status
            .code()
            .expect("missing exit code"),
        validate(home.path(), home.path(), Some(&malformed))
            .status
            .code()
            .expect("malformed exit code"),
        validate(home.path(), home.path(), Some(&unsupported))
            .status
            .code()
            .expect("unsupported exit code"),
        validate(home.path(), home.path(), Some(&invalid))
            .status
            .code()
            .expect("invalid exit code"),
    ];
    let distinct: BTreeSet<i32> = codes.iter().copied().collect();
    assert_eq!(distinct.len(), codes.len(), "exit codes collide: {codes:?}");
    assert_eq!(codes, [66, 3, 4, 5]);
}

/// A usage error is not a refusal about the document and must not share a
/// status with one. clap exits 2 from `Cli::parse()` before this command is
/// entered, which is why the unreadable class is `EX_NOINPUT` and not 2.
///
/// The pairwise test above cannot catch this: every invocation it makes
/// parses successfully, so it never reaches clap's exit path and stays green
/// while 2 means both "fix your command line" and "there is no document".
#[test]
fn a_usage_error_shares_no_status_with_any_refusal() {
    let home = TempDir::new().expect("home");
    let missing = home.path().join("missing.yaml");
    let malformed = home.path().join("malformed.yaml");
    fs::write(&malformed, MALFORMED_YAML).expect("write malformed goals");
    let unsupported = home.path().join("unsupported.yaml");
    fs::write(&unsupported, UNSUPPORTED_VERSION).expect("write unsupported-version goals");
    let invalid = home.path().join("invalid.yaml");
    fs::write(&invalid, DUPLICATE_GOAL).expect("write semantically invalid goals");

    let usage = ostrom(home.path())
        .args(["goals", "validate", "--no-such-flag"])
        .current_dir(home.path())
        .output()
        .expect("run ostrom goals validate with an unknown flag");
    let usage_code = usage.status.code().expect("usage exit code");
    assert_eq!(usage_code, 2, "{}", stderr(&usage));

    for (label, path) in [
        ("unreadable", &missing),
        ("malformed", &malformed),
        ("unsupported version", &unsupported),
        ("semantically invalid", &invalid),
    ] {
        let code = validate(home.path(), home.path(), Some(path))
            .status
            .code()
            .expect("refusal exit code");
        assert_ne!(
            code, usage_code,
            "the {label} refusal shares clap's usage status {usage_code}"
        );
    }
}

#[test]
fn repository_goals_take_precedence_over_the_config_root_and_the_config_root_is_the_fallback() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");
    let repository_dir = cwd.path().join(".ostrom");
    fs::create_dir_all(&repository_dir).expect("repository goals directory");
    let repository_goals = repository_dir.join("goals.yaml");
    let config_goals = home.path().join("goals.yaml");

    // The repository document is valid, the config root's is not. A wrong
    // precedence (config root wins) would make this refuse instead of
    // succeeding.
    fs::write(&repository_goals, VALID_GOALS).expect("write repository goals");
    fs::write(&config_goals, MALFORMED_YAML).expect("write config-root goals");
    let output = validate(home.path(), cwd.path(), None);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("valid: {}\n", repository_goals.display())
    );

    // Reverse the validity: the repository document is now invalid and the
    // config root's is valid. The repository still wins, so the exit code
    // must reflect ITS problem rather than silently falling back to the
    // valid config-root document.
    fs::write(&repository_goals, DUPLICATE_GOAL).expect("write repository goals");
    fs::write(&config_goals, VALID_GOALS).expect("write config-root goals");
    let output = validate(home.path(), cwd.path(), None);
    assert_eq!(output.status.code(), Some(5), "{}", stderr(&output));
    assert!(stdout(&output).is_empty());

    // With the repository file absent entirely, the config root is used.
    fs::remove_file(&repository_goals).expect("remove repository goals");
    let output = validate(home.path(), cwd.path(), None);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!("valid: {}\n", config_goals.display())
    );
}

#[test]
fn no_goals_document_anywhere_refuses_with_code_66_naming_both_locations() {
    let home = TempDir::new().expect("home");
    let cwd = TempDir::new().expect("cwd");

    let output = validate(home.path(), cwd.path(), None);

    assert_eq!(output.status.code(), Some(66), "{}", stderr(&output));
    assert!(stdout(&output).is_empty());
    let message = stderr(&output);
    let repository = cwd.path().join(".ostrom/goals.yaml");
    let config = home.path().join("goals.yaml");
    assert!(
        message.contains(repository.to_str().expect("UTF-8 path")),
        "{message}"
    );
    assert!(
        message.contains(config.to_str().expect("UTF-8 path")),
        "{message}"
    );
}

// --- Regression guard for the discover_goals_path refactor -----------------
//
// `ostrom plan` and `ostrom goals validate` now share one discovery
// function. This pins that sharing did not change `run_plan`'s existing
// "no file is a legitimate empty document" contract, which is deliberately
// different from the CLI validate command's refusal.

const ROSTER: &str = r#"
provider: file
cadence_hours: 1
stuck_after_days: 7
search_roots: []
hold_labels: []
bounce_all: []
projects:
  - repo: example-org/example-repo
    delegated: []
    excluded: []
    reserved: [10]
    default: delegated
    paused: false
    bounce: []
  - repo: another-example-org/another-example-repo
    delegated: [type:fix]
    excluded: []
    reserved: []
    default: excluded
    paused: false
    bounce: []
"#;

fn sweep_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sweep-cross-org.json")
}

fn configure_plan_home(home: &Path) {
    fs::write(home.join("ostrom.yaml"), "manifest_version: 1\n").expect("write repository policy");
    support::sign_manifest(&home.join("ostrom.yaml"));
    fs::write(home.join("mandates.yaml"), ROSTER).expect("write mandates");
    fs::write(
        home.join("gate.jsonl"),
        concat!(
            r#"{"ts":"2026-07-10T00:00:00Z","pr":"example-org/example-repo#1","head_sha":"0000000000000000000000000000000000000000","evidence":true,"verdict":"pass"}"#,
            "\n",
        ),
    )
    .expect("write gate evidence");
}

#[test]
fn ostrom_plan_still_tolerates_an_absent_goals_document() {
    let home = TempDir::new().expect("plan home");
    configure_plan_home(home.path());
    // Deliberately no goals.yaml anywhere.

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args([
            "plan",
            "--fixture",
            sweep_fixture().to_str().expect("fixture path is UTF-8"),
            "--started-at",
            "2026-08-01T00:00:00Z",
        ])
        .env("OSTROM_HOME", home.path())
        .env(
            "OSTROM_POLICY_TRUSTED_KEYS",
            home.path().join("trusted-policy-keys"),
        )
        .current_dir(home.path())
        .output()
        .expect("run ostrom plan");

    assert!(output.status.success(), "plan stderr: {}", stderr(&output));
    let document: serde_json::Value =
        serde_json::from_slice(&fs::read(home.path().join("plan.json")).expect("plan output"))
            .expect("parse plan");
    assert_eq!(document["goals"], serde_json::json!([]));
}
