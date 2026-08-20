#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, process::Command};

use tempfile::tempdir;

fn executable(path: &std::path::Path, source: &str) {
    fs::write(path, source).expect("write executable fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("make fixture executable");
}

#[test]
fn doctor_subcommand_reports_without_launching_node_or_npm() {
    let fixture = tempdir().expect("temporary doctor root");
    let plugin = fixture.path().join("plugin");
    let config = fixture.path().join("config");
    let home = fixture.path().join("home");
    let cwd = fixture.path().join("project");
    let bin = fixture.path().join("bin");
    for directory in [&plugin, &config, &home, &cwd, &bin] {
        fs::create_dir_all(directory).expect("doctor fixture directory");
    }
    fs::create_dir_all(plugin.join(".claude-plugin")).unwrap();
    fs::write(
        plugin.join(".claude-plugin/plugin.json"),
        r#"{"version":"1.30.15","minimumCliVersion":"0.4.0"}"#,
    )
    .unwrap();

    let node_marker = fixture.path().join("node-was-run");
    let npm_marker = fixture.path().join("npm-was-run");
    executable(
        &bin.join("node"),
        &format!("#!/bin/sh\ntouch '{}'\n", node_marker.display()),
    );
    executable(
        &bin.join("npm"),
        &format!("#!/bin/sh\ntouch '{}'\n", npm_marker.display()),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["doctor", "--check", "cli-installed"])
        .current_dir(&cwd)
        .env_clear()
        .env("HOME", &home)
        .env("CLAUDE_CONFIG_DIR", &config)
        .env("CLAUDE_PLUGIN_ROOT", &plugin)
        .env("PATH", &bin)
        .output()
        .expect("run native doctor");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "FAIL|cli-installed|ostrom is not installed or is absent from PATH|npm install -g @ostrom/cli\n"
    );
    assert!(output.stderr.is_empty());
    assert!(!node_marker.exists(), "native doctor must not launch Node");
    assert!(!npm_marker.exists(), "doctor must not execute its remedy");
}

#[test]
fn unknown_exact_check_exits_two() {
    let fixture = tempdir().expect("temporary doctor root");
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["doctor", "--check", "Environment"])
        .current_dir(fixture.path())
        .env_clear()
        .env("HOME", fixture.path())
        .env("CLAUDE_CONFIG_DIR", fixture.path().join("config"))
        .env("CLAUDE_PLUGIN_ROOT", fixture.path().join("plugin"))
        .output()
        .expect("run native doctor");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "unknown doctor check: Environment\n"
    );
}

#[test]
fn roster_gate_consistency_reports_roster_repository_missing_from_gate_end_to_end() {
    let fixture = tempdir().expect("temporary doctor root");
    let config = fixture.path().join("config");
    let ostrom_config = config.join("ostrom");
    let home = fixture.path().join("home");
    let cwd = fixture.path().join("project");
    for directory in [&ostrom_config, &home, &cwd] {
        fs::create_dir_all(directory).expect("doctor fixture directory");
    }
    fs::write(
        ostrom_config.join("mandates.yaml"),
        "provider: file\ncadence_hours: 1\nstuck_after_days: 7\nprojects:\n  - repo: placeholder-org/roster-only\n",
    )
    .expect("write mandate fixture");
    fs::write(
        ostrom_config.join("gate.yaml"),
        "provider: file\nprojects: []\n",
    )
    .expect("write gate fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["doctor", "--check", "roster-gate-consistency"])
        .current_dir(&cwd)
        .env_clear()
        .env("HOME", &home)
        .env("CLAUDE_CONFIG_DIR", &config)
        .output()
        .expect("run native doctor");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "FAIL|roster-gate-consistency|mandate roster repositories missing from gate.yaml projects: placeholder-org/roster-only|follow \"if you change one, change the other\": make the mandates.yaml roster and gate.yaml projects match; bounce/reserved mismatches are WARN because they may be deliberate, but should match unless documented\n"
    );
    assert!(output.stderr.is_empty());
}
