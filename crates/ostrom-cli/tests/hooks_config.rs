use std::{fs, process::Command, time::SystemTime};

use chrono::{DateTime, Utc};
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
    // The marker date and the command must agree on which day it is. Deriving
    // the marker from one instant and letting the binary read another lets a
    // UTC midnight between the two turn this into a flake that reproduces once
    // a day. Write the marker for both the day before and the day of, so the
    // roll cannot land between them.
    let start = DateTime::<Utc>::from(SystemTime::now());
    for day in [start - chrono::Duration::days(1), start] {
        let stamp = day.format("%Y-%m-%d").to_string();
        fs::write(fixture.path().join(format!(".tap-{stamp}")), "").unwrap();
    }

    let before = start.timestamp();
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .args(["hook", "digest"])
        .env("OSTROM_HOME", fixture.path())
        .current_dir(fixture.path())
        .output()
        .expect("render digest");
    let after = DateTime::<Utc>::from(SystemTime::now()).timestamp();
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
    let watermark = fs::read_to_string(fixture.path().join(".digest-decisions-read")).unwrap();
    let observed = DateTime::parse_from_rfc3339(watermark.trim())
        .expect("digest watermark timestamp")
        .timestamp();
    assert!(
        (before..=after).contains(&observed),
        "digest watermark {observed} is outside {before}..={after}"
    );
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
