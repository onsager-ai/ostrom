use std::{env, fs, path::PathBuf, process::Command};

use tempfile::tempdir;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay")
}

#[test]
fn rust_replay_is_byte_identical_to_recorded_shell_corpus() {
    let fixture = fixture_root();
    let root = tempdir().expect("temporary replay fixture");
    let home = root.path().join("home");
    let repo = root.path().join("repo");
    fs::create_dir_all(&home).expect("create replay home");
    fs::create_dir_all(&repo).expect("create replay repository");
    for name in ["mandates.yaml", "state.json", "selector-events.jsonl"] {
        fs::copy(fixture.join("home").join(name), home.join(name))
            .expect("install replay fixture file");
    }
    let fixture_bin = fixture.join("bin");
    let mut paths = vec![fixture_bin];
    paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    let path = env::join_paths(paths).expect("join fixture PATH");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["replay", "30"])
        .env("PATH", path)
        .env("OSTROM_HOME", &home)
        .env("MANDATE_REPLAY_TIME", "2026-08-01T00:00:00Z")
        .current_dir(repo)
        .output()
        .expect("run Rust replay");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        fs::read(fixture.join("replay.stdout")).expect("read shell-recorded output")
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn malformed_recorded_state_is_a_named_error() {
    let fixture = fixture_root();
    let root = tempdir().expect("temporary replay fixture");
    fs::copy(
        fixture.join("home/mandates.yaml"),
        root.path().join("mandates.yaml"),
    )
    .expect("install replay config");
    fs::write(root.path().join("state.json"), "not json\n").expect("write malformed state");
    let fixture_bin = fixture.join("bin");
    let mut paths = vec![fixture_bin];
    paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["replay", "30"])
        .env("PATH", env::join_paths(paths).expect("join fixture PATH"))
        .env("OSTROM_HOME", root.path())
        .env("MANDATE_REPLAY_TIME", "2026-08-01T00:00:00Z")
        .current_dir(root.path())
        .output()
        .expect("run Rust replay");

    assert_eq!(output.status.code(), Some(4));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("malformed sweep state"));
}
