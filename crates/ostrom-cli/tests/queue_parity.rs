use std::{fs, process::Command};

use serde_json::json;
use tempfile::tempdir;

#[test]
fn rust_queue_is_byte_identical_to_bash_over_runtime_input() {
    let fixture = tempdir().expect("temp dir");
    let ostrom_home = fixture.path().join("ostrom");
    fs::create_dir(&ostrom_home).expect("ostrom fixture dir");

    let mut queue = String::new();
    // Match the known production shape without capturing production content:
    // eleven invented repositories and seventy-nine invented rows are built
    // at runtime, so the parity harness is useful both in CI and when an
    // operator points OSTROM_HOME at a private local data set.
    for index in 0..79 {
        let project = index % 11;
        let kind = ["tripwire", "decision", "moved", "stuck", "drift"][index % 5];
        let state = match index % 3 {
            0 => "pending",
            1 => "deferred",
            _ => "approved",
        };
        let row = json!({
            "id": format!("synthetic-org/project-{project}#{}", index + 1),
            "repo": format!("synthetic-org/project-{project}"),
            "ref": format!("#{}", index + 1),
            "title": format!("Synthetic queue item {}", index + 1),
            "kind": kind,
            "mandate": {"reason": "synthetic placeholder reason"},
            "state": state,
            "opened": "2030-01-02T03:04:05Z",
            "age_days": index % 20,
            "aged_out": index % 20 >= 7,
            "needs_judgment": index % 2 == 0,
            "blocked_by": []
        });
        queue.push_str(&serde_json::to_string(&row).expect("serialize synthetic row"));
        queue.push('\n');
    }
    fs::write(ostrom_home.join("queue.jsonl"), queue).expect("write synthetic queue");

    let rust = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .env("OSTROM_HOME", &ostrom_home)
        .args(["queue", "list", "--format=json"])
        .output()
        .expect("run Rust reader");
    assert!(
        rust.status.success(),
        "Rust stderr: {}",
        String::from_utf8_lossy(&rust.stderr)
    );

    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root");
    let bash = Command::new("bash")
        .arg(repo_root.join("plugins/ostrom/scripts/queue.sh"))
        .arg("list")
        .env("CLAUDE_CONFIG_DIR", fixture.path())
        .env("CLAUDE_PLUGIN_ROOT", repo_root.join("plugins/ostrom"))
        .output()
        .expect("run Bash reader");
    assert!(
        bash.status.success(),
        "Bash stderr: {}",
        String::from_utf8_lossy(&bash.stderr)
    );
    assert_eq!(rust.stdout, bash.stdout, "queue parity invariant failed");
}
