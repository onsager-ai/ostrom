use std::{fs, process::Command};

use tempfile::tempdir;

fn ostrom() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ostrom"))
}

#[test]
fn install_writes_embedded_assets_byte_exactly_and_is_idempotent() {
    let root = tempdir().expect("protocol install root");
    let first = ostrom()
        .args(["install", "--root"])
        .arg(root.path())
        .env_clear()
        .output()
        .expect("first install");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_stdout = String::from_utf8(first.stdout).unwrap();
    assert_eq!(first_stdout.matches("wrote ").count(), 9, "{first_stdout}");

    let plugin = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/ostrom");
    for relative in [
        "hooks/hooks.json",
        "rules/frozen-rules.md",
        "skills/brief/SKILL.md",
        "skills/desk/SKILL.md",
        "skills/doctor/SKILL.md",
        "skills/gatekeep/SKILL.md",
        "skills/merge/SKILL.md",
        "skills/touch/SKILL.md",
        "skills/work/SKILL.md",
    ] {
        assert_eq!(
            fs::read(root.path().join(relative)).unwrap(),
            fs::read(plugin.join(relative)).unwrap(),
            "{relative}"
        );
    }

    let second = ostrom()
        .args(["install", "--root"])
        .arg(root.path())
        .env_clear()
        .output()
        .expect("second install");
    assert!(second.status.success());
    let second_stdout = String::from_utf8(second.stdout).unwrap();
    assert_eq!(
        second_stdout.matches("left-alone ").count(),
        9,
        "{second_stdout}"
    );
    assert!(
        second_stdout.contains("0 changed, 9 left alone"),
        "{second_stdout}"
    );
}

#[test]
fn verify_names_a_modified_asset_and_default_root_follows_claude_config() {
    let fixture = tempdir().expect("protocol fixture");
    let root = fixture.path().join("claude-config");
    let installed = ostrom()
        .arg("install")
        .env_clear()
        .env("CLAUDE_CONFIG_DIR", &root)
        .output()
        .expect("install into resolved harness root");
    assert!(installed.status.success());
    fs::write(root.join("skills/brief/SKILL.md"), "operator edit\n").unwrap();

    let verified = ostrom()
        .args(["install", "--verify"])
        .env_clear()
        .env("CLAUDE_CONFIG_DIR", &root)
        .output()
        .expect("verify installed protocol");
    assert_eq!(verified.status.code(), Some(1));
    assert!(
        String::from_utf8(verified.stdout)
            .unwrap()
            .contains("modified skills/brief/SKILL.md")
    );
}
