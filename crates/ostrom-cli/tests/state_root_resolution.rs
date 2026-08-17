//! The binary must find the state root an operator actually has.
//!
//! `ostrom queue list` reported zero rows against a live 130-row queue,
//! because every command but the most recently added ones resolved paths
//! through the store's XDG resolver, which deliberately refuses to fall
//! through to an operator's home. Nothing has run `ostrom migrate`, so the
//! real roster still lives where the shell wrote it.
//!
//! Zero rows and exit zero is also what an empty queue looks like, which is
//! why this went unnoticed: a broken read is indistinguishable from a quiet
//! portfolio unless something refuses.

use std::{fs, process::Command};

use tempfile::tempdir;

const ROW: &str = concat!(
    r##"{"id":"placeholder-org/alpha#1","repo":"placeholder-org/alpha","ref":"#1","##,
    r#""title":"Placeholder","kind":"decision","mandate":{"reason":"placeholder"},"#,
    r#""state":"pending","opened":"2026-08-01T00:00:00Z","age_days":0,"aged_out":false,"#,
    r#""needs_judgment":true,"blocked_by":[]}"#,
    "\n",
);

fn ostrom() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
    command.args(["queue", "list"]).env_remove("OSTROM_HOME");
    command
}

#[test]
fn claude_config_dir_locates_the_legacy_state_root() {
    let home = tempdir().expect("temporary CLAUDE_CONFIG_DIR");
    let root = home.path().join("ostrom");
    fs::create_dir(&root).expect("create legacy root");
    fs::write(root.join("queue.jsonl"), ROW).expect("write placeholder queue");

    let output = ostrom()
        .env("CLAUDE_CONFIG_DIR", home.path())
        .output()
        .expect("run queue list");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).lines().count(),
        1,
        "the row under CLAUDE_CONFIG_DIR/ostrom must be visible"
    );
}

#[test]
fn an_absent_state_root_refuses_rather_than_reporting_an_empty_queue() {
    let home = tempdir().expect("temporary home");
    let absent = home.path().join("nothing-here");

    let output = ostrom()
        .env("OSTROM_HOME", &absent)
        .output()
        .expect("run queue list");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        output.stdout.is_empty(),
        "a refusal must not also emit rows"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&absent.display().to_string()),
        "the refusal must name the directory it looked in: {stderr}"
    );
    assert_eq!(
        stderr.lines().count(),
        1,
        "one cause, one message: {stderr}"
    );
}

#[test]
fn an_explicit_root_still_wins_over_the_legacy_fallback() {
    let explicit = tempdir().expect("temporary OSTROM_HOME");
    fs::write(explicit.path().join("queue.jsonl"), ROW).expect("write placeholder queue");
    let decoy = tempdir().expect("temporary CLAUDE_CONFIG_DIR");
    fs::create_dir(decoy.path().join("ostrom")).expect("create decoy root");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["queue", "list"])
        .env("OSTROM_HOME", explicit.path())
        .env("CLAUDE_CONFIG_DIR", decoy.path())
        .output()
        .expect("run queue list");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 1);
}

#[test]
fn the_default_session_path_finds_the_legacy_root_under_home() {
    // This is the case an actual session takes: neither variable set, and the
    // roster still where the shell wrote it. It is the path the defect was in,
    // and it was the one path the first fix did not cover.
    let home = tempdir().expect("temporary HOME");
    let root = home.path().join(".claude/ostrom");
    fs::create_dir_all(&root).expect("create legacy root");
    fs::write(root.join("queue.jsonl"), ROW).expect("write placeholder queue");

    let output = ostrom()
        .env_remove("CLAUDE_CONFIG_DIR")
        .env("HOME", home.path())
        .output()
        .expect("run queue list");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 1);
}

#[test]
fn an_empty_claude_config_dir_reads_as_unset_not_as_a_relative_path() {
    // `PathBuf::from("").join("ostrom")` is the *relative* path `ostrom/`,
    // which would read whatever sits under the working directory. The shell
    // spelled this `${CLAUDE_CONFIG_DIR:-...}`; empty means unset.
    let home = tempdir().expect("temporary HOME");
    let root = home.path().join(".claude/ostrom");
    fs::create_dir_all(&root).expect("create legacy root");
    fs::write(root.join("queue.jsonl"), ROW).expect("write placeholder queue");

    let decoy = home.path().join("working/ostrom");
    fs::create_dir_all(&decoy).expect("create decoy relative root");
    fs::write(decoy.join("queue.jsonl"), "").expect("write empty decoy queue");

    let output = ostrom()
        .env("CLAUDE_CONFIG_DIR", "")
        .env("HOME", home.path())
        .current_dir(home.path().join("working"))
        .output()
        .expect("run queue list");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).lines().count(),
        1,
        "an empty value must fall through to the legacy root, not to ./ostrom"
    );
}
