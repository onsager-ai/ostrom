#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};

use serde_json::json;
use tempfile::tempdir;

fn executable(path: &std::path::Path, source: &str) {
    fs::write(path, source).expect("write executable fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("make fixture executable");
}

fn run_trace_completeness(root: &Path, config: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["doctor", "--check", "trace-completeness"])
        .current_dir(root)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("CLAUDE_CONFIG_DIR", config)
        .env("OSTROM_PLUGIN_ROOT", root.join("plugin"))
        .output()
        .expect("run trace-completeness doctor check")
}

fn gatekeeper_trace(
    owner: &str,
    timestamp: &str,
    item_selected: usize,
    verdicts: &[&str],
) -> String {
    let mut records = vec![json!({
        "ts": timestamp,
        "kind": "pass-started",
        "fact": {"owner": owner},
        "narration": {}
    })];
    for pr in 1..=item_selected {
        records.push(json!({
            "ts": timestamp,
            "kind": "item-selected",
            "fact": {"repo": "placeholder/example", "pr": pr},
            "narration": {}
        }));
    }
    for (index, verdict) in verdicts.iter().enumerate() {
        records.push(json!({
            "ts": timestamp,
            "kind": "gate-verdict-consumed",
            "fact": {
                "repo": "placeholder/example",
                "pr": index + 1,
                "head_sha": "placeholder",
                "verdict": verdict
            },
            "narration": {}
        }));
    }
    records.push(json!({
        "ts": timestamp,
        "kind": "pass-ended",
        "fact": {"owner": owner, "outcome": "completed"},
        "narration": {}
    }));
    format!(
        "{}\n",
        records
            .into_iter()
            .map(|record| serde_json::to_string(&record).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn write_trace(config: &Path, source: &str) {
    let ostrom = config.join("ostrom");
    fs::create_dir_all(&ostrom).expect("trace fixture directory");
    fs::write(ostrom.join("sprint.jsonl"), source).expect("trace fixture");
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
        .env("OSTROM_PLUGIN_ROOT", &plugin)
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
        .env("OSTROM_PLUGIN_ROOT", fixture.path().join("plugin"))
        .output()
        .expect("run native doctor");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "unknown doctor check: Environment\n"
    );
}

#[test]
fn full_doctor_names_every_registered_environment_variable() {
    let fixture = tempdir().expect("temporary doctor root");
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .arg("doctor")
        .current_dir(fixture.path())
        .env_clear()
        .env("HOME", fixture.path())
        .output()
        .expect("run full native doctor");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("doctor UTF-8 output");
    for variable in ostrom_store::ENVIRONMENT_VARIABLES {
        let prefix = format!("ENV|{}|class={}|set=", variable.name, variable.class);
        assert!(
            stdout.lines().any(|line| line.starts_with(&prefix)),
            "doctor omitted {}",
            variable.name
        );
    }
    assert!(stdout.contains("ENV|HOME|class=identity|set=yes|resolved="));
}

#[test]
fn trace_completeness_accepts_matching_counts_in_the_most_recent_pass() {
    let fixture = tempdir().expect("temporary doctor root");
    let config = fixture.path().join("config");
    let older = gatekeeper_trace("gatekeeper-placeholder-old", "2026-08-18T09:00:00Z", 3, &[]);
    let recent = gatekeeper_trace(
        "gatekeeper-placeholder-current",
        "2026-08-18T10:00:00Z",
        3,
        &["pass", "fail", "inconclusive"],
    );
    write_trace(&config, &format!("{older}{recent}"));

    let output = run_trace_completeness(fixture.path(), &config);

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "OK|trace-completeness|gatekeeper pass 2026-08-18T10:00:00Z: item-selected=3, gate-verdict-consumed=3|\n"
    );
}

#[test]
fn trace_completeness_fails_when_no_selected_verdicts_were_consumed() {
    let fixture = tempdir().expect("temporary doctor root");
    let config = fixture.path().join("config");
    write_trace(
        &config,
        &gatekeeper_trace(
            "gatekeeper-placeholder-current",
            "2026-08-18T10:00:00Z",
            3,
            &[],
        ),
    );

    let output = run_trace_completeness(fixture.path(), &config);

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "FAIL|trace-completeness|gatekeeper pass 2026-08-18T10:00:00Z: item-selected=3, gate-verdict-consumed=0|restart the gatekeeper session; it may be running a plugin older than the merge-side appends\n"
    );
}

#[test]
fn trace_completeness_accepts_a_pass_with_no_selections() {
    let fixture = tempdir().expect("temporary doctor root");
    let config = fixture.path().join("config");
    write_trace(
        &config,
        &gatekeeper_trace(
            "gatekeeper-placeholder-current",
            "2026-08-18T10:00:00Z",
            0,
            &[],
        ),
    );

    let output = run_trace_completeness(fixture.path(), &config);

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "OK|trace-completeness|gatekeeper pass 2026-08-18T10:00:00Z: item-selected=0, gate-verdict-consumed=0|\n"
    );
}

#[test]
fn trace_completeness_counts_fail_and_inconclusive_verdicts_before_failing_a_shortfall() {
    let fixture = tempdir().expect("temporary doctor root");
    let config = fixture.path().join("config");
    write_trace(
        &config,
        &gatekeeper_trace(
            "gatekeeper-placeholder-current",
            "2026-08-18T10:00:00Z",
            3,
            &["fail", "inconclusive"],
        ),
    );

    let output = run_trace_completeness(fixture.path(), &config);

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "FAIL|trace-completeness|gatekeeper pass 2026-08-18T10:00:00Z: item-selected=3, gate-verdict-consumed=2|restart the gatekeeper session; it may be running a plugin older than the merge-side appends\n"
    );
}

#[test]
fn trace_completeness_warns_for_missing_and_unreadable_traces() {
    let fixture = tempdir().expect("temporary doctor root");
    let missing_config = fixture.path().join("missing-config");
    let missing = run_trace_completeness(fixture.path(), &missing_config);
    assert!(missing.status.success());
    assert_eq!(
        String::from_utf8(missing.stdout).unwrap(),
        "WARN|trace-completeness|no gatekeeper pass ever recorded|run ostrom pass gatekeeper and confirm it records pass-ended\n"
    );

    let unreadable_config = fixture.path().join("unreadable-config");
    fs::create_dir_all(unreadable_config.join("ostrom/sprint.jsonl"))
        .expect("unreadable trace fixture");
    let unreadable = run_trace_completeness(fixture.path(), &unreadable_config);
    assert!(unreadable.status.success());
    assert_eq!(
        String::from_utf8(unreadable.stdout).unwrap(),
        "WARN|trace-completeness|gatekeeper pass history is unreadable|inspect sprint.jsonl and fix its permissions\n"
    );
}
