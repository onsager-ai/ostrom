#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use ostrom_core::WorkOrder;
use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    state: PathBuf,
    source: PathBuf,
    order_file: PathBuf,
    lease_file: PathBuf,
    codex: PathBuf,
    gh_as: PathBuf,
    unit: String,
}

impl Fixture {
    fn new(token_ceiling: u64) -> Self {
        let root = tempfile::tempdir().expect("temporary implementer fixture");
        let state = root.path().join("ostrom");
        let source = root.path().join("placeholder-alpha");
        fs::create_dir_all(&state).expect("create state");
        fs::create_dir_all(&source).expect("create source");
        git(&source, &["init", "-b", "main"]);
        git(
            &source,
            &["config", "user.email", "fixture@example.invalid"],
        );
        git(&source, &["config", "user.name", "Fixture"]);
        fs::write(source.join("README.md"), "placeholder\n").expect("write source");
        git(&source, &["add", "README.md"]);
        git(&source, &["commit", "-m", "base"]);
        git(
            &source,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/placeholder-org/alpha.git",
            ],
        );
        git(&source, &["update-ref", "refs/remotes/origin/main", "HEAD"]);

        let order = json!({
            "schema_version": 1,
            "item_id": "placeholder-org/alpha#7",
            "repository": "placeholder-org/alpha",
            "item_ref": "#7",
            "branch_name": "ostrom/placeholder-change",
            "spec": "Change the placeholder fixture.",
            "acceptance_criteria": ["The placeholder changes."],
            "constraints": ["Remain inside the fixture."],
            "order_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "created_at": "2026-08-01T00:00:00Z",
            "cost_ceiling_usd": 10,
            "token_ceiling": token_ceiling
        });
        let order_file = root.path().join("order.json");
        fs::write(
            &order_file,
            format!(
                "{}\n",
                serde_json::to_string(&order).expect("serialize order")
            ),
        )
        .expect("write order");
        let parsed = WorkOrder::from_json(&fs::read(&order_file).expect("read order"))
            .expect("valid work order");
        let lease_file = state.join(format!("implementer-item-{}.lease", parsed.item_hash()));
        let unit = "ostrom-implementer-placeholder".to_owned();

        let codex = root.path().join("codex-stub");
        executable(
            &codex,
            concat!(
                "worktree=\nresult=\n",
                "while [ \"$#\" -gt 0 ]; do\n",
                "  case \"$1\" in -C) worktree=$2; shift 2 ;; -o) result=$2; shift 2 ;; *) shift ;; esac\n",
                "done\n",
                "case \"${FAKE_CODEX_MODE:-complete}\" in\n",
                "  over) printf '%s\\n' '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":0,\"cached_input_tokens\":0,\"output_tokens\":101,\"reasoning_output_tokens\":0}}' ;;\n",
                "  wait)\n",
                "    printf '%s\\n' preserved >>\"$worktree/README.md\"\n",
                "    printf '%s\\n' \"$$\" >\"$OSTROM_HOME/codex.pid\"\n",
                "    (trap '' TERM; while :; do sleep 1; done) &\n",
                "    printf '%s\\n' \"$!\" >\"$OSTROM_HOME/codex-grandchild.pid\"\n",
                "    trap 'exit 143' TERM\n",
                "    while :; do sleep 1; done ;;\n",
                "  *)\n",
                "    printf '%s\\n' completed >>\"$worktree/README.md\"\n",
                "    printf '%s\\n' done >\"$result\"\n",
                "    printf '%s\\n' '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":10,\"cached_input_tokens\":5,\"output_tokens\":10,\"reasoning_output_tokens\":1}}' ;;\n",
                "esac"
            ),
        );
        let gh_as = root.path().join("gh-as-stub");
        executable(
            &gh_as,
            concat!(
                "while [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n",
                "shift\n",
                "if [ \"$1\" = gh ] && [ \"$2\" = repo ]; then printf '%s\\n' main; exit 0; fi\n",
                "if [ \"$1\" = gh ] && [ \"$2\" = pr ]; then printf '%s\\n' https://example.invalid/placeholder/pull/7; exit 0; fi\n",
                "if [ \"$1\" = git ] && [ \"$2\" = -C ] && printf '%s\\n' \"$*\" | grep -q ' fetch '; then\n",
                "  git -C \"$3\" update-ref refs/remotes/origin/main refs/heads/main; exit 0\n",
                "fi\n",
                "if [ \"$1\" = git ] && printf '%s\\n' \"$*\" | grep -q ' push '; then exit 0; fi\n",
                "exit 1"
            ),
        );
        Self {
            root,
            state,
            source,
            order_file,
            lease_file,
            codex,
            gh_as,
            unit,
        }
    }

    fn acquire(&self) {
        fs::write(
            &self.lease_file,
            format!(
                "{{\"owner\":\"{}\",\"started_at\":1,\"expires_at\":9999999999}}\n",
                self.unit
            ),
        )
        .expect("write lease");
    }

    fn command(&self, mode: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        command
            .arg("implement")
            .arg(&self.order_file)
            .arg(&self.unit)
            .env("OSTROM_HOME", &self.state)
            .env("CLAUDE_CONFIG_DIR", self.root.path())
            .env(
                "CLAUDE_PLUGIN_ROOT",
                env!("CARGO_MANIFEST_DIR").replace("crates/ostrom-cli", "plugins/ostrom"),
            )
            .env("MANDATE_IMPLEMENTER_SOURCE_REPO", &self.source)
            .env("MANDATE_GH_AS_BIN", &self.gh_as)
            .env("MANDATE_IMPLEMENTER_TERMINATION_GRACE_SECONDS", "1")
            .env("MANDATE_TRACE_TIME", "2026-08-01T00:00:00Z")
            .env("CODEX_BIN", &self.codex)
            .env("FAKE_CODEX_MODE", mode)
            .stdout(Stdio::null());
        command
    }

    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(self.state.join("sprint.jsonl"))
            .expect("read trace")
            .lines()
            .map(|line| serde_json::from_str(line).expect("trace JSON"))
            .collect()
    }
}

fn executable(path: &Path, body: &str) {
    fs::write(path, format!("#!/usr/bin/env bash\nset -eu\n{body}\n")).expect("write stub");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
}

fn git(path: &Path, arguments: &[&str]) {
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(path)
            .args(arguments)
            .status()
            .expect("run git")
            .success(),
        "git {arguments:?}"
    );
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if path.exists() && fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {}", path.display());
}

fn signal(pid: u32, name: &str) {
    assert!(
        Command::new("kill")
            .args([format!("-{name}"), pid.to_string()])
            .status()
            .expect("send signal")
            .success()
    );
}

fn wait(mut child: Child) -> ExitStatus {
    child.wait().expect("wait for implementer")
}

#[test]
fn token_ceiling_is_recorded_after_codex_completes() {
    let fixture = Fixture::new(100);
    fixture.acquire();
    let status = fixture.command("over").status().expect("run implementer");
    assert!(!status.success());
    assert!(!fixture.lease_file.exists());
    let terminal = fixture.trace().pop().expect("terminal trace");
    assert_eq!(terminal["kind"], "work-failed");
    assert_eq!(terminal["fact"]["reason"], "token-ceiling-exceeded");
    assert_eq!(terminal["fact"]["weighted_tokens"], 101);
}

#[test]
fn terminated_run_preserves_and_reuses_worktree_without_orphans() {
    let fixture = Fixture::new(100);
    fixture.acquire();
    let child = fixture.command("wait").spawn().expect("start implementer");
    wait_for(&fixture.state.join("codex-grandchild.pid"));
    signal(child.id(), "TERM");
    assert_eq!(wait(child).code(), Some(143));
    assert!(!fixture.lease_file.exists());
    let first = fixture.trace().pop().expect("failure trace");
    assert_eq!(first["fact"]["reason"], "signal-TERM");
    let worktree = PathBuf::from(
        first["fact"]["worktree_path"]
            .as_str()
            .expect("preserved path"),
    );
    assert!(worktree.exists());
    let grandchild = fs::read_to_string(fixture.state.join("codex-grandchild.pid"))
        .expect("read grandchild pid");
    assert!(
        !Command::new("kill")
            .args(["-0", grandchild.trim()])
            .stderr(Stdio::null())
            .status()
            .expect("probe grandchild")
            .success()
    );

    fixture.acquire();
    let output = fixture
        .command("complete")
        .output()
        .expect("retry implementer");
    assert!(output.status.success());
    let terminal = fixture.trace().pop().expect("completion trace");
    assert_eq!(terminal["kind"], "work-completed");
    assert!(worktree.exists());
    assert!(
        fs::read_to_string(worktree.join("README.md"))
            .expect("read preserved work")
            .contains("preserved")
    );
}
