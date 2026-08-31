#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::tempdir;

mod support;

const STARTED_AT: &str = "2026-08-01T00:05:00Z";
const ROSTER: &str = r#"provider: file
cadence_hours: 24
stuck_after_days: 7
projects:
  - repo: placeholder-org/alpha
    delegated: []
    excluded: []
    reserved: []
    default: excluded
    paused: false
    bounce: []
"#;
const ACQUIRED_FIXTURE: &str = r#"{"repositories":[{"repo":"placeholder-org/alpha","issues":[],"open_prs":[],"merged_prs":[],"default_branch":"main","branches":[],"branch_read_degraded":false,"ci_runs":[]}]}"#;
const PUBLISHED_GATE_FIXTURE: &str = concat!(
    r#"{"ts":"2026-04-01T00:00:00Z","pr":"placeholder-org/alpha#1","head_sha":"old-placeholder","verdict":"fail","already_judged":false,"conditions":[]}"#,
    "\n",
    r#"{"ts":"2026-07-30T00:00:00Z","pr":"placeholder-org/alpha#2","head_sha":"recent-placeholder","verdict":"pass","already_judged":false,"conditions":[]}"#,
    "\n",
    r#"{"ts":"2026-08-01T00:00:00Z","pr":"placeholder-org/alpha#3","head_sha":"latest-placeholder","verdict":"inconclusive","already_judged":false,"conditions":[]}"#,
    "\n",
);

fn write_sweep_fixture(root: &Path, body: &str) -> (PathBuf, PathBuf) {
    let home = root.join("ostrom-home");
    fs::create_dir(&home).expect("create scratch OSTROM_HOME");
    fs::write(home.join("ostrom.yaml"), "manifest_version: 1\n").expect("write repository policy");
    support::sign_manifest(&home.join("ostrom.yaml"));
    fs::write(home.join("mandates.yaml"), ROSTER).expect("write placeholder roster");
    let fixture = root.join("fixture.json");
    fs::write(&fixture, body).expect("write sweep fixture");
    (home, fixture)
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write fixture executable");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("make fixture executable");
}

fn path_with(directory: &Path) -> std::ffi::OsString {
    let mut paths = vec![directory.to_path_buf()];
    paths.extend(env::split_paths(&env::var_os("PATH").expect("test PATH")));
    env::join_paths(paths).expect("join test PATH")
}

fn command_spies(root: &Path) -> (PathBuf, PathBuf) {
    let bin = root.join("spy-bin");
    let log = root.join("destination-commands.log");
    fs::create_dir(&bin).expect("create spy directory");
    for command in ["git", "gh"] {
        executable(
            &bin.join(command),
            "#!/bin/sh\nprintf '%s %s\\n' \"$0\" \"$*\" >>\"$OSTROM_TEST_COMMAND_LOG\"\nexit 97\n",
        );
    }
    (bin, log)
}

struct LocalPublisher {
    remote: PathBuf,
    bin: PathBuf,
    gh_as: PathBuf,
    real_git: PathBuf,
}

impl LocalPublisher {
    fn apply(&self, command: &mut Command) {
        command
            .env("PATH", path_with(&self.bin))
            .env("MANDATE_GH_AS_BIN", &self.gh_as)
            .env("OSTROM_TEST_LOCAL_REMOTE", &self.remote)
            .env("OSTROM_TEST_REAL_GIT", &self.real_git);
    }
}

fn local_publisher(root: &Path) -> LocalPublisher {
    let remote = root.join("state.git");
    git(None, &["init", "--bare", "--quiet", path_text(&remote)]);
    let bin = root.join("publisher-bin");
    fs::create_dir(&bin).expect("create publisher adapter directory");
    executable(
        &bin.join("gh"),
        r#"#!/bin/sh
set -eu
[ "$1" = repo ]
[ "$2" = clone ]
exec "$OSTROM_TEST_REAL_GIT" clone --no-checkout "$OSTROM_TEST_LOCAL_REMOTE" "$4"
"#,
    );
    let gh_as = bin.join("credential-boundary.sh");
    executable(
        &gh_as,
        r#"#!/bin/sh
set -eu
shift 7
exec "$@"
"#,
    );
    LocalPublisher {
        remote,
        bin,
        gh_as,
        real_git: which("git").expect("find real git executable"),
    }
}

fn run_sweep(home: &Path, fixture: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
    command
        .args([
            "sweep",
            "--mode",
            "full",
            "--started-at",
            STARTED_AT,
            "--fixture",
        ])
        .arg(fixture)
        .env("OSTROM_HOME", home)
        .env(
            "OSTROM_POLICY_TRUSTED_KEYS",
            home.join("trusted-policy-keys"),
        )
        .current_dir(home);
    command
}

#[test]
fn inherited_publish_environment_cannot_enable_publication_without_the_typed_option() {
    let root = tempdir().expect("scratch publication boundary");
    let (home, fixture) = write_sweep_fixture(root.path(), ACQUIRED_FIXTURE);
    let (spy_bin, command_log) = command_spies(root.path());
    let output = run_sweep(&home, &fixture)
        .env("PATH", path_with(&spy_bin))
        .env("OSTROM_TEST_COMMAND_LOG", &command_log)
        .env("MANDATE_PUBLISH_REMOTE", "placeholder-org/forbidden-target")
        .output()
        .expect("run opted-out scratch sweep");

    assert!(
        output.status.success(),
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !command_log.exists(),
        "an inherited destination reached a destination-facing command"
    );
    assert!(!home.join("publish").exists());
}

#[test]
fn explicit_destination_publishes_only_to_a_local_remote() {
    let root = tempdir().expect("local publication remote");
    let (home, fixture) = write_sweep_fixture(root.path(), ACQUIRED_FIXTURE);
    fs::write(
        home.join("gate.jsonl"),
        concat!(
            r#"{"ts":"2026-08-01T00:00:00Z","pr":"placeholder-org/alpha#1","head_sha":"placeholder-sha","verdict":"pass","already_judged":false,"conditions":[]}"#,
            "\n",
        ),
    )
    .expect("write placeholder gate record");

    let remote = root.path().join("state.git");
    git(None, &["init", "--bare", "--quiet", path_text(&remote)]);
    let bin = root.path().join("bin");
    fs::create_dir(&bin).expect("create adapter directory");
    let gh_log = root.path().join("gh.log");
    let scope_log = root.path().join("scope.log");
    executable(
        &bin.join("gh"),
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >>"$OSTROM_TEST_GH_LOG"
[ "$1" = repo ]
[ "$2" = clone ]
[ "$3" = placeholder-org/alpha ]
[ "$5" = -- ]
[ "$6" = --no-checkout ]
exec "$OSTROM_TEST_REAL_GIT" clone --no-checkout "$OSTROM_TEST_LOCAL_REMOTE" "$4"
"#,
    );
    let gh_as = bin.join("credential-boundary.sh");
    executable(
        &gh_as,
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >>"$OSTROM_TEST_SCOPE_LOG"
[ "$1" = publisher ]
[ "$2" = placeholder-org/alpha ]
[ "$3" = --repositories ]
[ "$4" = placeholder-org/alpha ]
[ "$5" = --permissions ]
case "$6" in
  metadata:read,contents:read|metadata:read,contents:write) ;;
  *) exit 98 ;;
esac
[ "$7" = -- ]
shift 7
exec "$@"
"#,
    );
    let output = run_sweep(&home, &fixture)
        .args(["--publish-repository", "placeholder-org/alpha"])
        .env("PATH", path_with(&bin))
        .env("MANDATE_GH_AS_BIN", &gh_as)
        .env("OSTROM_TEST_GH_LOG", &gh_log)
        .env("OSTROM_TEST_SCOPE_LOG", &scope_log)
        .env("OSTROM_TEST_LOCAL_REMOTE", &remote)
        .env(
            "OSTROM_TEST_REAL_GIT",
            which("git").expect("find real git executable"),
        )
        .env("GIT_AUTHOR_NAME", "Ostrom Test")
        .env("GIT_AUTHOR_EMAIL", "ostrom@example.test")
        .env("GIT_COMMITTER_NAME", "Ostrom Test")
        .env("GIT_COMMITTER_EMAIL", "ostrom@example.test")
        .output()
        .expect("run explicit local publication");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("mandate publish: published"));
    assert_eq!(
        git_output_bare(&remote, &["rev-list", "--count", "state"]),
        "1"
    );
    let tree = git_output_bare(&remote, &["ls-tree", "-r", "--name-only", "state"]);
    assert!(tree.lines().any(|path| path == "manifest.json"));
    assert!(tree.lines().any(|path| path == "gate/2026-08-01.jsonl"));
    let scopes = fs::read_to_string(scope_log).expect("read credential scope log");
    assert!(scopes.contains("--permissions metadata:read,contents:read"));
    assert!(scopes.contains("--permissions metadata:read,contents:write"));
    assert_eq!(
        fs::read_to_string(gh_log).expect("read clone log").trim(),
        format!(
            "repo clone placeholder-org/alpha {} -- --no-checkout",
            home.join("publish").display()
        )
    );
}

#[test]
fn malformed_publish_allowlist_override_fails_before_destination_access() {
    let root = tempdir().expect("malformed publication allowlist");
    let (home, fixture) = write_sweep_fixture(root.path(), ACQUIRED_FIXTURE);
    let allowlist = root.path().join("malformed-allowlist.json");
    fs::write(&allowlist, "not json\n").expect("write malformed allowlist");
    let (spy_bin, command_log) = command_spies(root.path());

    let output = run_sweep(&home, &fixture)
        .args(["--publish-repository", "placeholder-org/alpha"])
        .env("PATH", path_with(&spy_bin))
        .env("OSTROM_TEST_COMMAND_LOG", &command_log)
        .env("MANDATE_PUBLISH_ALLOWLIST", &allowlist)
        .output()
        .expect("run publication with malformed override");

    assert!(
        output.status.success(),
        "publication failure must not fail reconciliation: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("publish failed; local records remain authoritative")
            && stderr.contains("invalid publication allowlist"),
        "stderr: {stderr}"
    );
    assert!(
        !command_log.exists(),
        "a malformed override fell back and reached the destination"
    );
}

#[test]
fn rejected_publication_keeps_the_successful_local_generation() {
    let root = tempdir().expect("rejected local publication remote");
    let (home, fixture) = write_sweep_fixture(root.path(), ACQUIRED_FIXTURE);
    let remote = root.path().join("rejecting-state.git");
    git(None, &["init", "--bare", "--quiet", path_text(&remote)]);
    let hooks = remote.join("hooks");
    let git_dir = format!("--git-dir={}", remote.display());
    git(
        None,
        &[&git_dir, "config", "core.hooksPath", path_text(&hooks)],
    );
    executable(
        &hooks.join("pre-receive"),
        "#!/bin/sh\nprintf 'placeholder push rejection\\n' >&2\nexit 1\n",
    );

    let bin = root.path().join("bin");
    fs::create_dir(&bin).expect("create adapter directory");
    executable(
        &bin.join("gh"),
        r#"#!/bin/sh
set -eu
[ "$1" = repo ]
[ "$2" = clone ]
exec "$OSTROM_TEST_REAL_GIT" clone --no-checkout "$OSTROM_TEST_LOCAL_REMOTE" "$4"
"#,
    );
    let gh_as = bin.join("credential-boundary.sh");
    executable(
        &gh_as,
        r#"#!/bin/sh
set -eu
shift 7
exec "$@"
"#,
    );
    let allowlist =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../ostrom-store/assets/publish-allowlist.json");
    let output = run_sweep(&home, &fixture)
        .args(["--publish-repository", "placeholder-org/alpha"])
        .env("PATH", path_with(&bin))
        .env("MANDATE_GH_AS_BIN", &gh_as)
        .env("MANDATE_PUBLISH_ALLOWLIST", &allowlist)
        .env("OSTROM_TEST_LOCAL_REMOTE", &remote)
        .env(
            "OSTROM_TEST_REAL_GIT",
            which("git").expect("find real git executable"),
        )
        .env("GIT_AUTHOR_NAME", "Ostrom Test")
        .env("GIT_AUTHOR_EMAIL", "ostrom@example.test")
        .env("GIT_COMMITTER_NAME", "Ostrom Test")
        .env("GIT_COMMITTER_EMAIL", "ostrom@example.test")
        .output()
        .expect("run sweep with rejected publication");

    assert!(
        output.status.success(),
        "publication failure must not fail reconciliation: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(home.join("queue.jsonl").is_file());
    assert!(home.join("state.json").is_file());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("publish failed; local records remain authoritative"),
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        git_output_bare_optional(&remote, &["show-ref", "--verify", "refs/heads/state"]).is_none()
    );
}

#[test]
fn publication_recovers_dirty_and_invalid_checkouts_without_losing_local_records() {
    let root = tempdir().expect("recoverable publication fixture");
    let (home, fixture) = write_sweep_fixture(root.path(), ACQUIRED_FIXTURE);
    let unpublished_gate = concat!(
        r#"{"ts":"2026-08-01T00:00:00Z","pr":"placeholder-org/alpha#475","head_sha":"unpublished-placeholder","verdict":"pass","already_judged":false,"conditions":[]}"#,
        "\n",
    );
    fs::write(home.join("gate.jsonl"), unpublished_gate)
        .expect("write authoritative unpublished record");
    let publisher = local_publisher(root.path());
    let empty_git_config = root.path().join("empty-gitconfig");
    fs::write(&empty_git_config, "").expect("write empty git config");

    let mut first = run_sweep(&home, &fixture);
    publisher.apply(&mut first);
    let first_output = first
        .args(["--publish-repository", "placeholder-org/alpha"])
        .env("GIT_CONFIG_GLOBAL", &empty_git_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL")
        .output()
        .expect("run publication whose commit fails");
    assert!(first_output.status.success());
    assert!(
        String::from_utf8_lossy(&first_output.stderr).contains("publish failed"),
        "stderr: {}",
        String::from_utf8_lossy(&first_output.stderr)
    );
    assert_eq!(
        Command::new("git")
            .arg("-C")
            .arg(home.join("publish"))
            .args(["diff", "--cached", "--quiet"])
            .status()
            .expect("inspect staged residue")
            .code(),
        Some(1),
        "the fixture must reproduce staged residue"
    );

    run_recovery_sweep(&home, &fixture, &publisher);
    assert_eq!(
        git_output_bare(&publisher.remote, &["rev-list", "--count", "state"]),
        "1"
    );
    assert_eq!(
        git_output_bare(&publisher.remote, &["show", "state:gate/2026-08-01.jsonl"]),
        unpublished_gate.trim(),
        "checkout recovery discarded a record authoritative on the volume"
    );

    fs::write(home.join("publish/manifest.json"), "staged residue\n")
        .expect("write staged residue");
    git(Some(&home.join("publish")), &["add", "manifest.json"]);
    run_recovery_sweep(&home, &fixture, &publisher);

    fs::write(home.join("publish/unrelated.tmp"), "untracked residue\n")
        .expect("write untracked residue");
    run_recovery_sweep(&home, &fixture, &publisher);
    assert!(!home.join("publish/unrelated.tmp").exists());

    fs::remove_dir_all(home.join("publish")).expect("replace fixture checkout");
    fs::create_dir(home.join("publish")).expect("create invalid checkout");
    fs::write(home.join("publish/unknown-record"), "preserve me\n")
        .expect("write unknown volume content");
    run_recovery_sweep(&home, &fixture, &publisher);
    assert_eq!(
        fs::read_to_string(home.join("publish.invalid-1/unknown-record"))
            .expect("read quarantined volume content"),
        "preserve me\n"
    );
    assert_eq!(
        git_output_bare(&publisher.remote, &["show", "state:gate/2026-08-01.jsonl"]),
        unpublished_gate.trim()
    );
}

#[test]
fn absent_gate_source_preserves_published_partitions_rollup_and_counts() {
    let root = tempdir().expect("gate preservation publication fixture");
    let (home, fixture) = write_sweep_fixture(root.path(), ACQUIRED_FIXTURE);
    fs::write(home.join("gate.jsonl"), PUBLISHED_GATE_FIXTURE)
        .expect("write authoritative gate fixture");
    let publisher = local_publisher(root.path());

    run_local_publication(&home, &fixture, &publisher);

    let first_gate_tree = git_output_bare(&publisher.remote, &["ls-tree", "-r", "state", "gate"]);
    assert!(first_gate_tree.contains("gate/2026-07-30.jsonl"));
    assert!(first_gate_tree.contains("gate/2026-08-01.jsonl"));
    assert!(!first_gate_tree.contains("gate/2026-04-01.jsonl"));
    let first_rollup = git_json_bare(&publisher.remote, "state:rollup.json");
    assert_eq!(first_rollup["verdicts_by_day"]["2026-04-01"]["fail"], 1);
    let first_manifest = git_json_bare(&publisher.remote, "state:manifest.json");
    assert_eq!(first_manifest["record_counts"]["gate"], 2);
    assert_eq!(first_manifest["record_counts"]["gate_partitions"], 2);

    fs::remove_file(home.join("gate.jsonl")).expect("remove gate source from publishing host");
    fs::write(
        home.join("mandates.yaml"),
        ROSTER.replace("cadence_hours: 24", "cadence_hours: 12"),
    )
    .expect("change a non-gate publication input");
    run_local_publication(&home, &fixture, &publisher);

    assert_eq!(
        git_output_bare(&publisher.remote, &["rev-list", "--count", "state"]),
        "2",
        "the second run must publish while gate is absent"
    );
    assert_eq!(
        git_output_bare(&publisher.remote, &["ls-tree", "-r", "state^", "gate"]),
        git_output_bare(&publisher.remote, &["ls-tree", "-r", "state", "gate"]),
        "preserved gate partitions were rewritten or removed"
    );
    let second_rollup = git_json_bare(&publisher.remote, "state:rollup.json");
    assert_eq!(
        second_rollup["verdicts_by_day"], first_rollup["verdicts_by_day"],
        "the forever-retained gate rollup was not preserved"
    );
    let second_manifest = git_json_bare(&publisher.remote, "state:manifest.json");
    assert_eq!(second_manifest["record_counts"]["gate"], 2);
    assert_eq!(second_manifest["record_counts"]["gate_partitions"], 2);
}

#[test]
fn present_empty_gate_source_authoritatively_clears_published_gate() {
    let root = tempdir().expect("authoritative empty gate publication fixture");
    let (home, fixture) = write_sweep_fixture(root.path(), ACQUIRED_FIXTURE);
    fs::write(home.join("gate.jsonl"), PUBLISHED_GATE_FIXTURE)
        .expect("write authoritative gate fixture");
    let publisher = local_publisher(root.path());

    run_local_publication(&home, &fixture, &publisher);
    assert!(!git_output_bare(&publisher.remote, &["ls-tree", "-r", "state", "gate"]).is_empty());

    fs::write(home.join("gate.jsonl"), "").expect("write present empty gate source");
    fs::write(
        home.join("mandates.yaml"),
        ROSTER.replace("cadence_hours: 24", "cadence_hours: 12"),
    )
    .expect("change a non-gate publication input");
    run_local_publication(&home, &fixture, &publisher);

    assert_eq!(
        git_output_bare(&publisher.remote, &["rev-list", "--count", "state"]),
        "2"
    );
    assert!(
        git_output_bare(&publisher.remote, &["ls-tree", "-r", "state", "gate"]).is_empty(),
        "a present empty authoritative source did not clear gate partitions"
    );
    let rollup = git_json_bare(&publisher.remote, "state:rollup.json");
    assert_eq!(rollup["verdicts_by_day"], serde_json::json!({}));
    let manifest = git_json_bare(&publisher.remote, "state:manifest.json");
    assert_eq!(manifest["record_counts"]["gate"], 0);
    assert_eq!(manifest["record_counts"]["gate_partitions"], 0);
}

fn run_local_publication(home: &Path, fixture: &Path, publisher: &LocalPublisher) {
    let mut command = run_sweep(home, fixture);
    publisher.apply(&mut command);
    let output = command
        .args(["--publish-repository", "placeholder-org/alpha"])
        .env("GIT_AUTHOR_NAME", "Ostrom Test")
        .env("GIT_AUTHOR_EMAIL", "ostrom@example.test")
        .env("GIT_COMMITTER_NAME", "Ostrom Test")
        .env("GIT_COMMITTER_EMAIL", "ostrom@example.test")
        .output()
        .expect("run local publication");
    assert!(
        output.status.success()
            && !String::from_utf8_lossy(&output.stderr).contains("publish failed"),
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_recovery_sweep(home: &Path, fixture: &Path, publisher: &LocalPublisher) {
    let mut command = run_sweep(home, fixture);
    publisher.apply(&mut command);
    let output = command
        .args(["--publish-repository", "placeholder-org/alpha"])
        .env("GIT_AUTHOR_NAME", "Ostrom Test")
        .env("GIT_AUTHOR_EMAIL", "ostrom@example.test")
        .env("GIT_COMMITTER_NAME", "Ostrom Test")
        .env("GIT_COMMITTER_EMAIL", "ostrom@example.test")
        .output()
        .expect("run recovered publication");
    assert!(
        output.status.success()
            && !String::from_utf8_lossy(&output.stderr).contains("publish failed"),
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(directory: Option<&Path>, arguments: &[&str]) -> Output {
    let mut command = Command::new("git");
    if let Some(directory) = directory {
        command.arg("-C").arg(directory);
    }
    let output = command.args(arguments).output().expect("run fixture git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git_output_bare(remote: &Path, arguments: &[&str]) -> String {
    let mut command = Command::new("git");
    let output = command
        .arg(format!("--git-dir={}", remote.display()))
        .args(arguments)
        .output()
        .expect("inspect local bare repository");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git_output_bare_optional(remote: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg(format!("--git-dir={}", remote.display()))
        .args(arguments)
        .output()
        .expect("inspect optional local bare ref");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_json_bare(remote: &Path, revision: &str) -> serde_json::Value {
    serde_json::from_str(&git_output_bare(remote, &["show", revision]))
        .expect("published file is valid JSON")
}

fn path_text(path: &Path) -> &str {
    path.to_str().expect("fixture path is UTF-8")
}

fn which(command: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        let candidate = directory.join(command);
        candidate.is_file().then_some(candidate)
    })
}
