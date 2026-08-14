use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use serde_json::json;
use tempfile::tempdir;

fn run_readers(ostrom_home: &Path, claude_config_dir: &Path) -> (Output, Output) {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root");
    let rust = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .env("OSTROM_HOME", ostrom_home)
        .args(["queue", "list", "--format=json"])
        .output()
        .expect("run Rust reader");
    let bash = Command::new("bash")
        .arg(repo_root.join("plugins/ostrom/scripts/queue.sh"))
        .arg("list")
        .env("CLAUDE_CONFIG_DIR", claude_config_dir)
        .env("CLAUDE_PLUGIN_ROOT", repo_root.join("plugins/ostrom"))
        .output()
        .expect("run Bash reader");
    (rust, bash)
}

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

    let (rust, bash) = run_readers(&ostrom_home, fixture.path());
    assert!(
        rust.status.success(),
        "Rust stderr: {}",
        String::from_utf8_lossy(&rust.stderr)
    );
    assert!(
        bash.status.success(),
        "Bash stderr: {}",
        String::from_utf8_lossy(&bash.stderr)
    );
    assert_eq!(rust.stdout, bash.stdout, "queue parity invariant failed");

    let rendered = rust
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<serde_json::Value>(line).expect("rendered queue row"))
        .collect::<Vec<_>>();
    assert!(
        rendered.iter().any(|row| row["needs_judgment"] == true),
        "parity fixture must exercise a judgment classification"
    );
    assert!(
        rendered.iter().any(|row| row["needs_judgment"] == false),
        "parity fixture must exercise a non-judgment classification"
    );
}

#[test]
fn rust_and_bash_reject_the_same_malformed_blocked_by_rows() {
    let fixture = tempdir().expect("temp dir");
    let ostrom_home = fixture.path().join("ostrom");
    fs::create_dir(&ostrom_home).expect("ostrom fixture dir");

    for (case, blocked_by) in [
        ("owner contains hash", "synthetic#org/project#1"),
        ("repository contains hash", "synthetic-org/pro#ject#1"),
        ("issue number starts at zero", "synthetic-org/project#0"),
        (
            "repository has an extra segment",
            "synthetic-org/group/project#1",
        ),
        (
            "repository contains whitespace",
            "synthetic-org/project name#1",
        ),
    ] {
        let row = json!({
            "id": "synthetic-org/project#1",
            "repo": "synthetic-org/project",
            "ref": "#1",
            "title": "Synthetic malformed queue item",
            "kind": "decision",
            "mandate": {"reason": "synthetic placeholder reason"},
            "state": "pending",
            "opened": "2030-01-02T03:04:05Z",
            "blocked_by": [blocked_by]
        });
        fs::write(
            ostrom_home.join("queue.jsonl"),
            format!(
                "{}\n",
                serde_json::to_string(&row).expect("serialize malformed row")
            ),
        )
        .expect("write malformed queue");

        let (rust, bash) = run_readers(&ostrom_home, fixture.path());
        let rust_rejected = !rust.status.success();
        let bash_rejected = !bash.status.success();
        assert_eq!(
            rust_rejected,
            bash_rejected,
            "reader disagreement for {case}; Rust stderr: {}; Bash stderr: {}",
            String::from_utf8_lossy(&rust.stderr),
            String::from_utf8_lossy(&bash.stderr)
        );
        assert!(
            rust_rejected,
            "both readers accepted malformed case: {case}"
        );
    }
}
