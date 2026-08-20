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
    repository_visibility: String,
    unit: String,
}

impl Fixture {
    fn new(token_ceiling: u64) -> Self {
        Self::with_repository_visibility(token_ceiling, "public")
    }

    fn with_repository_visibility(token_ceiling: u64, repository_visibility: &str) -> Self {
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
        fs::create_dir_all(source.join(".github/workflows")).expect("create workflow fixture");
        fs::write(
            source.join(".github/workflows/existing.yml"),
            "name: baseline\n",
        )
        .expect("write baseline workflow");
        git(
            &source,
            &["add", "README.md", ".github/workflows/existing.yml"],
        );
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
                "  usage) printf '%s\\n' 'Usage: codex exec [OPTIONS]' >&2; exit 2 ;;\n",
                "  config) printf '%s\\n' 'Error loading config.toml: placeholder configuration' >&2; exit 1 ;;\n",
                "  config-required) printf '%s\\n' 'Error: features.placeholder is required when fixture is enabled' >&2; exit 1 ;;\n",
                "  model-failure) exit 1 ;;\n",
                "  partial-failure)\n",
                "    printf '%s\\n' partial >\"$worktree/partial.txt\"\n",
                "    printf '%s\\n' \"$FAKE_CODEX_USAGE_JSON\"\n",
                "    exit 1 ;;\n",
                "  workflow-only)\n",
                "    mkdir -p \"$worktree/.github/workflows\"\n",
                "    printf '%s\\n' 'name: placeholder' >\"$worktree/.github/workflows/test.yml\"\n",
                "    printf '%s\\n' done >\"$result\"\n",
                "    printf '%s\\n' '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":10,\"cached_input_tokens\":5,\"output_tokens\":10,\"reasoning_output_tokens\":1}}' ;;\n",
                "  workflow-mixed)\n",
                "    mkdir -p \"$worktree/.github/workflows\"\n",
                "    printf '%s\\n' 'name: changed' >\"$worktree/.github/workflows/existing.yml\"\n",
                "    printf '%s\\n' 'name: placeholder' >\"$worktree/.github/workflows/new.yml\"\n",
                "    printf '%s\\n' ordinary >>\"$worktree/README.md\"\n",
                "    printf '%s\\n' done >\"$result\"\n",
                "    printf '%s\\n' '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":10,\"cached_input_tokens\":5,\"output_tokens\":10,\"reasoning_output_tokens\":1}}' ;;\n",
                "  conflict)\n",
                "    printf '%s\\n' 'implementer version' >\"$worktree/base.txt\"\n",
                "    printf '%s\\n' done >\"$result\"\n",
                "    printf '%s\\n' '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":10,\"cached_input_tokens\":5,\"output_tokens\":10,\"reasoning_output_tokens\":1}}' ;;\n",
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
                "    usage=${FAKE_CODEX_USAGE_JSON:-}\n",
                "    if [ -z \"$usage\" ]; then usage='{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":10,\"cached_input_tokens\":5,\"output_tokens\":10,\"reasoning_output_tokens\":1}}'; fi\n",
                "    printf '%s\\n' \"$usage\" ;;\n",
                "esac"
            ),
        );
        let gh_as = root.path().join("gh-as-stub");
        executable(
            &gh_as,
            concat!(
                "permissions=\n",
                "while [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do\n",
                "  case \"$1\" in --permissions) permissions=$2; shift 2 ;; *) shift ;; esac\n",
                "done\n",
                "shift\n",
                "if [ \"$1\" = gh ] && [ \"$2\" = repo ]; then\n",
                "  case \"${FAKE_REPOSITORY_VISIBILITY:-public}:$permissions\" in\n",
                "    public:metadata:read|public:metadata:read,contents:read|private:metadata:read,contents:read) printf '%s\\n' main; exit 0 ;;\n",
                "    *) exit 1 ;;\n",
                "  esac\n",
                "fi\n",
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
            repository_visibility: repository_visibility.to_owned(),
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
            .env_clear()
            .env("OSTROM_HOME", &self.state)
            .env("CLAUDE_CONFIG_DIR", self.root.path())
            .env("HOME", self.root.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env(
                "CLAUDE_PLUGIN_ROOT",
                env!("CARGO_MANIFEST_DIR").replace("crates/ostrom-cli", "plugins/ostrom"),
            )
            .env("MANDATE_IMPLEMENTER_SOURCE_REPO", &self.source)
            .env("MANDATE_GH_AS_BIN", &self.gh_as)
            .env("FAKE_REPOSITORY_VISIBILITY", &self.repository_visibility)
            .env("MANDATE_IMPLEMENTER_TERMINATION_GRACE_SECONDS", "1")
            .env("MANDATE_TRACE_TIME", "2026-08-01T00:00:00Z")
            .env("MANDATE_NOW_EPOCH", "1785542400")
            .env("MANDATE_TODAY", "2026-08-01")
            .env("MANDATE_SWEEP_TIME", "2026-08-01T00:00:00Z")
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

    fn record_dispatch(&self) {
        let order = WorkOrder::from_json(&fs::read(&self.order_file).expect("read order"))
            .expect("valid order");
        fs::write(
            self.state.join("sprint.jsonl"),
            format!(
                "{}\n",
                json!({
                    "ts": "2026-08-01T00:00:00Z",
                    "kind": "work-dispatched",
                    "fact": {
                        "schema_version": 1,
                        "item_id": order.item_id,
                        "order_id": order.order_id,
                        "unit_name": self.unit,
                        "backend": "systemd",
                        "cost_ceiling_usd": order.cost_ceiling_usd,
                        "token_ceiling": order.token_ceiling
                    },
                    "narration": {}
                })
            ),
        )
        .expect("record dispatch");
    }

    fn worktree(&self) -> PathBuf {
        let order = WorkOrder::from_json(&fs::read(&self.order_file).expect("read order"))
            .expect("valid work order");
        self.state
            .join("implementer-worktrees")
            .join(order.item_hash())
    }
}

/// Stubs carry a shebang and mode 755 because the credential wrapper is now
/// executed directly rather than through `bash` — which is the whole point of
/// `MANDATE_GH_AS_BIN` being a path override, and would silently regress if a
/// stub were ever written without them.
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

fn git_output(path: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .output()
        .expect("run git");
    assert!(output.status.success(), "git {arguments:?}");
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_owned()
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

fn wait_for_child(pid: u32) -> u32 {
    let children = PathBuf::from(format!("/proc/{pid}/task/{pid}/children"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(child) = fs::read_to_string(&children)
            .ok()
            .and_then(|value| value.split_whitespace().next()?.parse().ok())
        {
            return child;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for child of {pid}");
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
fn public_repository_reaches_codex_and_uses_the_rust_cli_publication_boundary() {
    let fixture = Fixture::new(100);
    fixture.acquire();
    let output = fixture
        .command("complete")
        .stdout(Stdio::piped())
        .output()
        .expect("run implementer");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)
            .expect("UTF-8 output")
            .trim(),
        "https://example.invalid/placeholder/pull/7"
    );
    assert!(!fixture.lease_file.exists());
    let worktree = fixture.worktree();
    assert!(
        fs::read_to_string(worktree.join("README.md"))
            .expect("read implementation")
            .contains("completed")
    );
    let terminal = fixture.trace().pop().expect("completion trace");
    assert_eq!(terminal["kind"], "work-completed");
    assert_eq!(
        terminal["fact"]["pr_url"],
        "https://example.invalid/placeholder/pull/7"
    );
    let body = fixture
        .state
        .join("implementer-runs")
        .join(terminal["fact"]["order_id"].as_str().expect("order id"))
        .join("pr-body.md");
    let body = fs::read_to_string(body).expect("read pull request body");
    assert!(body.contains("workspace-write"));
    assert!(body.contains("approval policy `never`"));
    assert!(body.contains("Ostrom-Role: builder"));
}

#[test]
fn private_repository_reaches_codex_with_contents_read_scope() {
    let fixture = Fixture::with_repository_visibility(100, "private");
    fixture.acquire();

    let status = fixture
        .command("complete")
        .status()
        .expect("run implementer");

    assert!(status.success());
    assert!(
        fs::read_to_string(fixture.worktree().join("README.md"))
            .expect("read implementation")
            .contains("completed"),
        "Codex must run after default-branch resolution"
    );
    let terminal = fixture.trace().pop().expect("completion trace");
    assert_eq!(terminal["kind"], "work-completed");
    assert_ne!(terminal["fact"]["reason"], "default-branch-query-failed");
}

#[test]
fn source_resolution_prefers_a_primary_clone_over_a_linked_worktree() {
    let fixture = Fixture::new(100);
    let linked_root = fixture.root.path().join("linked-root");
    let linked = linked_root.join("linked");
    fs::create_dir(&linked_root).expect("create linked root");
    git(
        &fixture.source,
        &[
            "worktree",
            "add",
            "-b",
            "fixture/linked",
            &linked.display().to_string(),
            "refs/remotes/origin/main",
        ],
    );
    fs::write(
        fixture.state.join("mandates.yaml"),
        format!(
            "search_roots:\n  - {}\n  - {}\n",
            linked_root.display(),
            fixture.source.display()
        ),
    )
    .expect("write config");
    fixture.acquire();
    let status = fixture
        .command("complete")
        .env_remove("MANDATE_IMPLEMENTER_SOURCE_REPO")
        .status()
        .expect("run implementer");
    assert!(status.success());
    let terminal = fixture.trace().pop().expect("completion trace");
    assert_eq!(
        terminal["fact"]["source_repository_path"],
        fixture.source.display().to_string()
    );
}

#[test]
fn linked_worktree_only_source_is_named_and_creates_no_item_worktree() {
    let fixture = Fixture::new(100);
    let linked_root = fixture.root.path().join("linked-root");
    let linked = linked_root.join("linked");
    fs::create_dir(&linked_root).expect("create linked root");
    git(
        &fixture.source,
        &[
            "worktree",
            "add",
            "-b",
            "fixture/linked",
            &linked.display().to_string(),
            "refs/remotes/origin/main",
        ],
    );
    fs::write(
        fixture.state.join("mandates.yaml"),
        format!("search_roots:\n  - {}\n", linked_root.display()),
    )
    .expect("write config");
    fixture.acquire();
    let status = fixture
        .command("complete")
        .env_remove("MANDATE_IMPLEMENTER_SOURCE_REPO")
        .status()
        .expect("run implementer");
    assert!(!status.success());
    assert!(!fixture.lease_file.exists());
    assert!(!fixture.worktree().exists());
    let terminal = fixture.trace().pop().expect("failure trace");
    assert_eq!(
        terminal["fact"]["reason"],
        "source-repository-linked-worktree-only"
    );
    assert_eq!(
        terminal["fact"]["message"],
        format!(
            "source repository was found only as a linked worktree: {}",
            linked.display()
        )
    );
}

#[test]
fn branch_owned_by_another_worktree_is_rejected_without_reuse() {
    let fixture = Fixture::new(100);
    let order = WorkOrder::from_json(&fs::read(&fixture.order_file).expect("read order"))
        .expect("valid order");
    let existing = fixture.root.path().join("external-worktree");
    git(
        &fixture.source,
        &[
            "worktree",
            "add",
            "-b",
            &order.branch_name,
            &existing.display().to_string(),
            "refs/remotes/origin/main",
        ],
    );
    fixture.acquire();
    let status = fixture
        .command("complete")
        .status()
        .expect("run implementer");
    assert!(!status.success());
    assert!(!fixture.lease_file.exists());
    assert!(!fixture.worktree().exists());
    let terminal = fixture.trace().pop().expect("failure trace");
    assert_eq!(terminal["fact"]["reason"], "worktree-branch-already-exists");
    assert_eq!(
        terminal["fact"]["message"],
        format!(
            "branch {} already exists outside the item worktree: {}",
            order.branch_name,
            existing.display()
        )
    );
}

#[test]
fn workflow_only_change_is_withheld_and_never_published() {
    let fixture = Fixture::new(100);
    fixture.acquire();
    let status = fixture
        .command("workflow-only")
        .status()
        .expect("run implementer");
    assert!(!status.success());
    assert!(!fixture.lease_file.exists());
    let terminal = fixture.trace().pop().expect("failure trace");
    assert_eq!(terminal["fact"]["reason"], "workflow-file-unpushable");
    assert_eq!(
        terminal["fact"]["message"],
        "only workflow files changed; withheld paths: .github/workflows/test.yml"
    );
    assert_eq!(
        terminal["fact"]["withheld_paths"],
        json!([".github/workflows/test.yml"])
    );
}

#[test]
fn mixed_workflow_edits_are_withheld_while_ordinary_work_is_published() {
    let fixture = Fixture::new(100);
    fixture.acquire();
    let status = fixture
        .command("workflow-mixed")
        .status()
        .expect("run mixed workflow implementer");
    assert!(status.success());
    assert!(!fixture.lease_file.exists());
    let worktree = fixture.worktree();
    assert_eq!(
        fs::read_to_string(worktree.join(".github/workflows/existing.yml"))
            .expect("read restored workflow"),
        "name: baseline\n"
    );
    assert!(!worktree.join(".github/workflows/new.yml").exists());
    assert!(
        fs::read_to_string(worktree.join("README.md"))
            .expect("read ordinary work")
            .contains("ordinary")
    );
    let terminal = fixture.trace().pop().expect("completion trace");
    assert_eq!(terminal["kind"], "work-completed");
    assert_eq!(
        terminal["fact"]["withheld_paths"],
        json!([
            ".github/workflows/existing.yml",
            ".github/workflows/new.yml"
        ])
    );
}

#[test]
fn a_clean_historical_worktree_is_retargeted_to_the_order_branch() {
    let fixture = Fixture::new(100);
    let worktree = fixture.worktree();
    fs::create_dir_all(worktree.parent().expect("worktree parent"))
        .expect("create worktree parent");
    git(
        &fixture.source,
        &[
            "worktree",
            "add",
            "-b",
            "old/placeholder-order",
            worktree.to_str().expect("UTF-8 worktree path"),
            "refs/remotes/origin/main",
        ],
    );
    fixture.acquire();
    assert!(
        fixture
            .command("complete")
            .status()
            .expect("run retargeted implementer")
            .success()
    );
    let order = WorkOrder::from_json(&fs::read(&fixture.order_file).expect("read order"))
        .expect("valid order");
    assert_eq!(
        git_output(&worktree, &["branch", "--show-current"]),
        order.branch_name
    );
    assert_eq!(
        fixture.trace().pop().expect("completion trace")["kind"],
        "work-completed"
    );
}

#[test]
fn codex_invocation_failures_are_named_and_release_the_lease() {
    for (mode, code, reason) in [
        ("usage", 2, "codex-invocation-invalid"),
        ("config", 1, "codex-invocation-invalid"),
        ("config-required", 1, "codex-invocation-invalid"),
        ("model-failure", 1, "codex-exit-1"),
    ] {
        let fixture = Fixture::new(100);
        fixture.acquire();
        let status = fixture.command(mode).status().expect("run implementer");
        assert_eq!(status.code(), Some(code));
        assert!(!fixture.lease_file.exists());
        assert_eq!(
            fixture.trace().pop().expect("failure trace")["fact"]["reason"],
            reason
        );
    }
}

#[test]
fn unavailable_codex_interpreter_is_named_and_releases_the_lease() {
    let fixture = Fixture::new(100);
    fs::write(
        &fixture.codex,
        "#!/usr/bin/env missing-placeholder-runtime\n",
    )
    .expect("write broken Codex");
    fs::set_permissions(&fixture.codex, fs::Permissions::from_mode(0o755))
        .expect("chmod broken Codex");
    fixture.acquire();
    let status = fixture
        .command("complete")
        .status()
        .expect("run implementer");
    assert_eq!(status.code(), Some(127));
    assert!(!fixture.lease_file.exists());
    assert_eq!(
        fixture.trace().pop().expect("failure trace")["fact"]["reason"],
        "codex-unavailable"
    );
}

#[test]
fn conflicting_published_branch_is_named_and_aborts_the_merge() {
    let fixture = Fixture::new(100);
    let order = WorkOrder::from_json(&fs::read(&fixture.order_file).expect("read order"))
        .expect("valid order");
    let remote = fixture.root.path().join("origin.git");
    fs::create_dir(&remote).expect("create remote");
    git(&remote, &["init", "--bare"]);
    git(
        &fixture.source,
        &["push", &remote.display().to_string(), "main:main"],
    );
    let publisher = fixture.root.path().join("publisher");
    assert!(
        Command::new("git")
            .args([
                "clone",
                "--branch",
                "main",
                &remote.display().to_string(),
                &publisher.display().to_string(),
            ])
            .status()
            .expect("clone publisher")
            .success()
    );
    git(
        &publisher,
        &["config", "user.email", "fixture@example.invalid"],
    );
    git(&publisher, &["config", "user.name", "Fixture"]);
    git(&publisher, &["switch", "-c", &order.branch_name]);
    fs::write(publisher.join("base.txt"), "published version\n").expect("write published change");
    git(&publisher, &["add", "base.txt"]);
    git(&publisher, &["commit", "-m", "published placeholder"]);
    git(&publisher, &["push", "origin", &order.branch_name]);
    let remote_head = git_output(&publisher, &["rev-parse", "HEAD"]);

    executable(
        &fixture.gh_as,
        concat!(
            "while [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n",
            "shift\n",
            "if [ \"$1\" = gh ] && [ \"$2\" = repo ]; then printf '%s\\n' main; exit 0; fi\n",
            "if [ \"$1\" = gh ] && [ \"$2\" = pr ]; then : >\"$FAKE_PR_MARKER\"; exit 1; fi\n",
            "args=()\n",
            "for value in \"$@\"; do\n",
            "  if [ \"$value\" = https://github.com/placeholder-org/alpha.git ]; then value=$FAKE_GIT_REMOTE; fi\n",
            "  args+=(\"$value\")\n",
            "done\n",
            "exec \"${args[@]}\""
        ),
    );
    let pr_marker = fixture.root.path().join("pr-called");
    fixture.acquire();
    let status = fixture
        .command("conflict")
        .env("FAKE_GIT_REMOTE", &remote)
        .env("FAKE_PR_MARKER", &pr_marker)
        .status()
        .expect("run implementer");
    assert_eq!(status.code(), Some(1));
    assert!(!fixture.lease_file.exists());
    assert!(!pr_marker.exists());
    let worktree = fixture.worktree();
    assert_eq!(
        fs::read_to_string(worktree.join("base.txt")).expect("read restored change"),
        "implementer version\n"
    );
    assert!(
        !Command::new("git")
            .args([
                "-C",
                &worktree.display().to_string(),
                "rev-parse",
                "-q",
                "--verify",
                "MERGE_HEAD",
            ])
            .status()
            .expect("inspect merge state")
            .success()
    );
    let terminal = fixture.trace().pop().expect("failure trace");
    assert_eq!(terminal["fact"]["reason"], "branch-conflicted");
    assert_eq!(terminal["fact"]["remote_head_sha"], remote_head);
    assert_eq!(terminal["fact"]["conflicted_paths"], json!(["base.txt"]));
}

#[test]
fn independently_advanced_published_branch_is_merged_and_retried_once() {
    let fixture = Fixture::new(100);
    let order = WorkOrder::from_json(&fs::read(&fixture.order_file).expect("read order"))
        .expect("valid order");
    let remote = fixture.root.path().join("origin.git");
    fs::create_dir(&remote).expect("create remote");
    git(&remote, &["init", "--bare"]);
    git(
        &fixture.source,
        &["push", &remote.display().to_string(), "main:main"],
    );
    let publisher = fixture.root.path().join("publisher");
    assert!(
        Command::new("git")
            .args([
                "clone",
                "--branch",
                "main",
                &remote.display().to_string(),
                &publisher.display().to_string(),
            ])
            .status()
            .expect("clone publisher")
            .success()
    );
    git(
        &publisher,
        &["config", "user.email", "fixture@example.invalid"],
    );
    git(&publisher, &["config", "user.name", "Fixture"]);
    git(&publisher, &["switch", "-c", &order.branch_name]);
    fs::write(publisher.join("published.txt"), "published independently\n")
        .expect("write published change");
    git(&publisher, &["add", "published.txt"]);
    git(&publisher, &["commit", "-m", "published placeholder"]);
    git(&publisher, &["push", "origin", &order.branch_name]);

    let calls = fixture.root.path().join("credential-calls");
    executable(
        &fixture.gh_as,
        concat!(
            "while [ \"$#\" -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n",
            "shift\n",
            "printf '%s\\n' \"$*\" >>\"$FAKE_CALLS\"\n",
            "if [ \"$1\" = gh ] && [ \"$2\" = repo ]; then printf '%s\\n' main; exit 0; fi\n",
            "if [ \"$1\" = gh ] && [ \"$2\" = pr ]; then printf '%s\\n' https://example.invalid/placeholder/pull/7; exit 0; fi\n",
            "args=()\n",
            "for value in \"$@\"; do\n",
            "  if [ \"$value\" = https://github.com/placeholder-org/alpha.git ]; then value=$FAKE_GIT_REMOTE; fi\n",
            "  args+=(\"$value\")\n",
            "done\n",
            "exec \"${args[@]}\""
        ),
    );
    fixture.acquire();
    let output = fixture
        .command("complete")
        .env("FAKE_GIT_REMOTE", &remote)
        .env("FAKE_CALLS", &calls)
        .output()
        .expect("run merge-forward implementer");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = fs::read_to_string(calls).expect("read credential calls");
    assert_eq!(
        calls.lines().filter(|line| line.contains(" push ")).count(),
        2
    );
    assert_eq!(
        calls
            .lines()
            .filter(|line| {
                line.contains(" fetch ")
                    && line.contains(&format!("refs/heads/{}", order.branch_name))
            })
            .count(),
        1
    );
    assert_eq!(
        git_output(
            &remote,
            &["show", &format!("{}:published.txt", order.branch_name)]
        ),
        "published independently"
    );
    assert!(
        git_output(
            &remote,
            &["show", &format!("{}:README.md", order.branch_name)]
        )
        .contains("completed")
    );
    let parents = git_output(
        &remote,
        &["rev-list", "--parents", "-n", "1", &order.branch_name],
    );
    assert_eq!(parents.split_whitespace().count(), 3);
    assert_eq!(
        fixture.trace().pop().expect("completion trace")["kind"],
        "work-completed"
    );
}

#[test]
fn cached_token_accounting_and_failure_preservation_use_reported_components() {
    let completed = Fixture::new(500_000);
    completed.acquire();
    let status = completed
        .command("complete")
        .env(
            "FAKE_CODEX_USAGE_JSON",
            r#"{"type":"turn.completed","usage":{"input_tokens":4360176,"cached_input_tokens":4215296,"output_tokens":22074,"reasoning_output_tokens":0}}"#,
        )
        .status()
        .expect("run cached implementer");
    assert!(status.success());
    let terminal = completed.trace().pop().expect("completion trace");
    assert_eq!(terminal["fact"]["weighted_tokens"], 135_356);
    assert_eq!(terminal["fact"]["usage"]["fresh_input_tokens"], 144_880);
    assert_eq!(terminal["fact"]["usage"]["cached_input_tokens"], 4_215_296);

    let failed = Fixture::new(500_000);
    failed.acquire();
    let status = failed
        .command("partial-failure")
        .env(
            "FAKE_CODEX_USAGE_JSON",
            r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":50,"reasoning_output_tokens":10}}"#,
        )
        .status()
        .expect("run failed implementer");
    assert!(!status.success());
    let terminal = failed.trace().pop().expect("failure trace");
    assert_eq!(terminal["fact"]["reason"], "codex-exit-1");
    assert_eq!(
        terminal["fact"]["worktree_path"],
        failed.worktree().display().to_string()
    );
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
    assert_eq!(first["fact"]["termination_signal"], "SIGKILL");
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

#[test]
fn supervisor_terminalizes_a_dispatched_worker_killed_before_drop() {
    let fixture = Fixture::new(100);
    fixture.acquire();
    fixture.record_dispatch();
    let child = fixture.command("wait").spawn().expect("start implementer");
    wait_for(&fixture.state.join("codex-grandchild.pid"));
    let worker = wait_for_child(child.id());
    signal(worker, "KILL");
    assert_eq!(wait(child).code(), Some(1));

    for pid_file in ["codex.pid", "codex-grandchild.pid"] {
        if let Ok(pid) = fs::read_to_string(fixture.state.join(pid_file)) {
            let _ = Command::new("kill").args(["-KILL", pid.trim()]).status();
        }
    }

    assert!(!fixture.lease_file.exists());
    let trace = fixture.trace();
    assert_eq!(trace.len(), 2);
    assert_eq!(trace[0]["kind"], "work-dispatched");
    assert_eq!(trace[1]["kind"], "work-failed");
    assert_eq!(trace[1]["fact"]["reason"], "unit-exit-without-terminal");
    assert_eq!(trace[1]["fact"]["termination_signal"], "SIG9");
}
