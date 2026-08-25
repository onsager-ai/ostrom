use std::{env, fs, path::PathBuf, process::Command};

use chrono::{DateTime, Utc};
use ostrom_store::{OstromPaths, ReplayOptions, replay};
use tempfile::tempdir;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay")
}

#[test]
fn rust_replay_matches_recorded_behavior_corpus() {
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

    let output_path = root.path().join("replay.output");
    let output = Command::new(env::current_exe().expect("current integration test executable"))
        .args(["--exact", "injected_clock_replay_child"])
        .env("PATH", path)
        .env("OSTROM_TEST_REPLAY_HOME", &home)
        .env("OSTROM_TEST_REPLAY_CWD", &repo)
        .env("OSTROM_TEST_REPLAY_OUTPUT", &output_path)
        .output()
        .expect("run Rust replay");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    if env::var_os("OSTROM_BLESS_REPLAY").is_some() {
        fs::write(
            fixture.join("replay.stdout"),
            fs::read(&output_path).unwrap(),
        )
        .expect("record migrated replay output");
    }
    assert_eq!(
        fs::read(output_path).expect("read injected-clock replay output"),
        fs::read(fixture.join("replay.stdout")).expect("read recorded output")
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn injected_clock_replay_child() {
    let Some(home) = env::var_os("OSTROM_TEST_REPLAY_HOME") else {
        return;
    };
    let working_directory =
        PathBuf::from(env::var_os("OSTROM_TEST_REPLAY_CWD").expect("replay cwd"));
    let output_path =
        PathBuf::from(env::var_os("OSTROM_TEST_REPLAY_OUTPUT").expect("replay output"));
    let replay_time = DateTime::parse_from_rfc3339("2026-08-01T00:00:00Z")
        .expect("fixed replay timestamp")
        .with_timezone(&Utc);
    let home = PathBuf::from(home);
    let output = replay(&ReplayOptions {
        paths: OstromPaths {
            config: home.clone(),
            state: home,
        },
        working_directory,
        days: 30,
        replay_time,
    })
    .expect("run fixed-clock replay");
    fs::write(output_path, output).expect("write injected-clock replay output");
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
        .current_dir(root.path())
        .output()
        .expect("run Rust replay");

    assert_eq!(output.status.code(), Some(4));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("malformed sweep state"));
}
