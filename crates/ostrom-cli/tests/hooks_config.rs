use std::{fs, path::Path, process::Command};

use serde_json::Value;
use tempfile::tempdir;

#[test]
fn session_start_constitution_matches_the_retired_shell_bytes() {
    let fixture = tempdir().expect("temporary hook fixture");
    let plugin = fixture.path().join("plugin");
    let home = fixture.path().join("home");
    let user = home.join(".claude/ostrom");
    let repository = fixture.path().join("repository");
    fs::create_dir_all(plugin.join("rules")).unwrap();
    fs::create_dir_all(user.join("rules.d")).unwrap();
    fs::create_dir_all(repository.join(".ostrom/rules.d")).unwrap();
    fs::write(plugin.join("rules/frozen-rules.md"), "SHIPPED\n").unwrap();
    fs::write(user.join("rules.md"), "<!-- seeded only -->\n").unwrap();
    fs::write(user.join("rules.d/10-user.md"), "USER RULE\n").unwrap();
    fs::write(repository.join(".ostrom/rules.md"), "REPO RULE\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["hook", "session-start"])
        .env("OSTROM_HOME", &user)
        .env("CLAUDE_PLUGIN_ROOT", &plugin)
        .env("HOME", &home)
        .current_dir(&repository)
        .output()
        .expect("render constitution");
    assert!(output.status.success());
    let expected = concat!(
        "SHIPPED\n",
        "\n",
        "<!-- constitution: layers below override the shipped rules above on conflict -->\n",
        "\n",
        "<!-- constitution layer: user (~/.claude/ostrom/rules.d/10-user.md) -->\n",
        "\n",
        "USER RULE\n",
        "\n",
        "<!-- constitution layer: repo (./.ostrom/rules.md) -->\n",
        "\n",
        "REPO RULE\n",
    );
    assert_eq!(output.stdout, expected.as_bytes());
    assert!(output.stderr.is_empty());
}

#[test]
fn digest_envelope_matches_the_retired_shell_bytes() {
    let fixture = tempdir().expect("temporary digest fixture");
    fs::write(
        fixture.path().join("mandates.yaml"),
        r#"provider: file
cadence_hours: 24
stuck_after_days: 7
search_roots: []
bounce_all: []
projects:
  - repo: placeholder-org/alpha
    delegated: []
    excluded: []
    reserved: []
    default: delegated
    paused: false
    bounce: []
  - repo: placeholder-org/beta
    delegated: []
    excluded: []
    reserved: []
    default: delegated
    paused: false
    bounce: []
"#,
    )
    .unwrap();
    fs::write(fixture.path().join("queue.jsonl"), "").unwrap();
    fs::write(
        fixture.path().join("state.json"),
        "{\"version\":2,\"repos\":{}}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["hook", "digest"])
        .env("OSTROM_HOME", fixture.path())
        .env("MANDATE_NOW_EPOCH", "0")
        .env("MANDATE_TODAY", "not-a-date")
        .env("MANDATE_DIGEST_TIME", "2026-08-19T00:00:00Z")
        .current_dir(fixture.path())
        .output()
        .expect("render digest");
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        concat!(
            "{\n",
            "  \"systemMessage\": \"DECISIONS TAKEN: nothing since your last read\\n2 projects nominal\",\n",
            "  \"hookSpecificOutput\": {\n",
            "    \"hookEventName\": \"SessionStart\",\n",
            "    \"additionalContext\": \"DECISIONS TAKEN: nothing since your last read\\n2 projects nominal\"\n",
            "  }\n",
            "}\n",
        )
        .as_bytes()
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(fixture.path().join(".digest-decisions-read")).unwrap(),
        "2026-08-19T00:00:00Z\n"
    );
}

#[test]
fn digest_surfaces_repeated_dispatch_failure_escalations() {
    let fixture = tempdir().expect("temporary digest fixture");
    fs::write(
        fixture.path().join("mandates.yaml"),
        r#"provider: file
cadence_hours: 24
stuck_after_days: 7
search_roots: []
bounce_all: []
projects:
  - repo: placeholder-org/alpha
    delegated: []
    excluded: []
    reserved: []
    default: delegated
    paused: false
    bounce: []
"#,
    )
    .unwrap();
    fs::write(fixture.path().join("queue.jsonl"), "").unwrap();
    fs::write(
        fixture.path().join("state.json"),
        "{\"version\":2,\"repos\":{}}\n",
    )
    .unwrap();
    fs::write(
        fixture.path().join("sprint.jsonl"),
        concat!(
            r#"{"ts":"2026-08-19T01:00:00Z","kind":"dispatch-failure-escalated","fact":{"schema_version":1,"item_id":"placeholder-org/alpha#7","order_id":"placeholder-order","action":"suppress-dispatch","failure_reason":"branch-already-pushed","failure_count":2},"narration":{"reason":"Repeated failure.","conclusion":"Dispatch suppressed."}}"#,
            "\n",
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["hook", "digest"])
        .env("OSTROM_HOME", fixture.path())
        .env("MANDATE_NOW_EPOCH", "0")
        .env("MANDATE_TODAY", "not-a-date")
        .env("MANDATE_DIGEST_TIME", "2026-08-19T02:00:00Z")
        .current_dir(fixture.path())
        .output()
        .expect("render digest");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 digest");
    assert!(stdout.contains("DISPATCH FAILURES ESCALATED"));
    assert!(stdout.contains(
        "placeholder-org/alpha#7 — branch-already-pushed (2 identical failures; dispatch suppressed)"
    ));
}

#[test]
fn config_prints_the_same_compact_layered_roster_as_mandate_lib() {
    let fixture = tempdir().expect("temporary config fixture");
    let repository = fixture.path().join("repository");
    fs::create_dir_all(repository.join(".ostrom")).unwrap();
    fs::write(
        fixture.path().join("mandates.yaml"),
        r#"cadence_hours: 12
search_roots: [/placeholder/user]
bounce_all: [label:user]
projects:
  - repo: placeholder-org/alpha
    max_implementers_per_repository: 2
    delegated: [label:work]
    excluded: []
    reserved: [17]
    default: delegated
    paused: false
    bounce: []
"#,
    )
    .unwrap();
    fs::write(
        repository.join(".ostrom/mandates.yaml"),
        "stuck_after_days: 2\nsearch_roots: [/placeholder/repo]\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .arg("config")
        .env("OSTROM_HOME", fixture.path())
        .current_dir(&repository)
        .output()
        .expect("resolve config");
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        br#"{"provider":"file","cadence_hours":12,"stuck_after_days":2,"search_roots":["/placeholder/repo"],"hold_labels":[],"work_ranking":[],"bounce_all":["label:user"],"projects":[{"repo":"placeholder-org/alpha","paused":false,"default":"delegated","delegated":["label:work"],"excluded":[],"reserved":[17],"bounce":[],"max_implementers_per_repository":2}]}
"#
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn shipped_hook_commands_are_silent_when_ostrom_is_absent() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let document: Value = serde_json::from_slice(
        &fs::read(repository.join("plugins/ostrom/hooks/hooks.json")).unwrap(),
    )
    .unwrap();
    let empty_path = tempdir().unwrap();
    for hook in document["hooks"]["SessionStart"].as_array().unwrap() {
        let command = hook["hooks"][0]["command"].as_str().unwrap();
        let output = Command::new("/bin/sh")
            .args(["-c", command])
            .env_clear()
            .env("PATH", empty_path.path())
            .output()
            .expect("run hook command without CLI");
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}
