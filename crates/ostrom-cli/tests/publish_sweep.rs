#![cfg(unix)]

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::tempdir;

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

fn write_sweep_fixture(root: &Path, body: &str) -> (PathBuf, PathBuf) {
    let home = root.join("ostrom-home");
    fs::create_dir(&home).expect("create scratch OSTROM_HOME");
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
    let gh_as = bin.join("gh-as.sh");
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
    let allowlist = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/ostrom/config/publish-allowlist.json");
    let output = run_sweep(&home, &fixture)
        .args(["--publish-repository", "placeholder-org/alpha"])
        .env("PATH", path_with(&bin))
        .env("MANDATE_GH_AS_BIN", &gh_as)
        .env("MANDATE_PUBLISH_ALLOWLIST", &allowlist)
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
    let gh_as = bin.join("gh-as.sh");
    executable(
        &gh_as,
        r#"#!/bin/sh
set -eu
shift 7
exec "$@"
"#,
    );
    let allowlist = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/ostrom/config/publish-allowlist.json");
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

fn path_text(path: &Path) -> &str {
    path.to_str().expect("fixture path is UTF-8")
}

fn which(command: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        let candidate = directory.join(command);
        candidate.is_file().then_some(candidate)
    })
}
