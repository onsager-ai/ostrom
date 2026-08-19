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
