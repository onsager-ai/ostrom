use std::{fs, path::Path, process::Command};

use tempfile::TempDir;

mod support;

const LOCAL_POLICY: &str = "manifest_version: 1\nactors: {builder: {}, gatekeeper: {}}\noperations:\n  local-proof:\n    name: Local proof\n    steps:\n      - uses: cmd/run\n        with:\n          script: 'test -z \"$GH_TOKEN\" && test -z \"$GITHUB_TOKEN\" && printf local-ok'\ngrants:\n  builder-local: {actors: builder, operations: local-proof, repositories: placeholder-org/repo}\n";

fn ostrom(home: &Path) -> Command {
    let trusted_keys = support::sign_manifest(&home.join("policy.yaml"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
    command
        .env("OSTROM_HOME", home)
        .env("OSTROM_POLICY_MANIFEST", home.join("policy.yaml"))
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted_keys)
        .env("OSTROM_ACTOR", "builder");
    command
}

fn fixture(policy: &str) -> TempDir {
    let root = TempDir::new().expect("operation fixture");
    fs::write(root.path().join("policy.yaml"), policy).expect("write policy");
    root
}

fn prompt_policy(prompt: &str, declarations: &str) -> String {
    format!(
        "manifest_version: 1\nactors: {{builder: {{}}}}\n{declarations}operations:\n  inspect:\n    steps:\n      - uses: agent/claude\n        with:\n          prompt: {prompt}\ngrants:\n  builder-inspect: {{actors: builder, operations: inspect, repositories: placeholder-org/repo}}\nloops:\n  inspection-loop:\n    actor: builder\n    operation: inspect\n    target: placeholder-org/repo\n    every: hourly\n"
    )
}

#[test]
fn local_operation_runs_without_inherited_forge_credentials() {
    let root = fixture(LOCAL_POLICY);
    let output = ostrom(root.path())
        .env("GH_TOKEN", "placeholder-token")
        .env("GITHUB_TOKEN", "placeholder-token")
        .args(["local-proof", "placeholder-org/repo"])
        .output()
        .expect("run operation");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"local-ok");
}

#[test]
fn repository_operation_redefinition_is_not_adopted_for_execution() {
    let repository = TempDir::new().expect("repository fixture");
    let home = TempDir::new().expect("operator fixture");
    fs::create_dir(repository.path().join(".git")).expect("repository boundary");
    let repository_manifest = repository.path().join("ostrom.yaml");
    fs::write(
        &repository_manifest,
        "manifest_version: 1\nactors: {builder: {}}\noperations:\n  local-proof:\n    steps:\n      - uses: cmd/run\n        with: {script: 'printf repository'}\ngrants:\n  repository-grant: {actors: builder, operations: local-proof, repositories: placeholder-org/repo}\n",
    )
    .expect("write repository declaration");
    let operator_manifest = home.path().join("ostrom.yaml");
    fs::write(
        &operator_manifest,
        "manifest_version: 1\nactors: {builder: {}}\noperations:\n  local-proof:\n    steps:\n      - uses: cmd/run\n        with: {script: 'printf operator'}\n",
    )
    .expect("write adopted operation");
    let trusted_keys = support::sign_manifest(&repository_manifest);
    support::sign_manifest(&operator_manifest);

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .current_dir(repository.path())
        .env("OSTROM_HOME", home.path())
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted_keys)
        .env("OSTROM_ACTOR", "builder")
        .args(["local-proof", "placeholder-org/repo"])
        .output()
        .expect("run adopted operation");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"operator");
}

#[cfg(unix)]
#[test]
fn pull_request_authority_is_loaded_from_the_base_commit() {
    use std::os::unix::fs::PermissionsExt as _;

    let repository = TempDir::new().expect("repository fixture");
    let home = TempDir::new().expect("operator fixture");
    let git = |arguments: &[&str]| {
        let output = Command::new("git")
            .current_dir(repository.path())
            .args(arguments)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "placeholder@example.com"]);
    git(&["config", "user.name", "Placeholder"]);
    let repository_manifest = repository.path().join("ostrom.yaml");
    fs::write(repository.path().join("README.md"), "base\n").expect("write base repository");
    git(&["add", "README.md"]);
    git(&["commit", "-qm", "base without policy"]);
    let base_sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
        .expect("base SHA UTF-8")
        .trim()
        .to_owned();

    fs::write(
        &repository_manifest,
        "manifest_version: 1\ngrants:\n  head-only-grant: {actors: builder, operations: local-proof, repositories: placeholder-org/repo}\n",
    )
    .expect("write head grant");
    let trusted_keys = support::sign_manifest(&repository_manifest);
    git(&["add", "ostrom.yaml", "ostrom.yaml.sig"]);
    git(&["commit", "-qm", "add head grant"]);

    let marker = repository.path().join("head-ran");
    let operator_manifest = home.path().join("ostrom.yaml");
    fs::write(
        &operator_manifest,
        format!(
            "manifest_version: 1\nactors: {{builder: {{}}}}\noperations:\n  local-proof:\n    steps:\n      - uses: cmd/run\n        with: {{script: 'printf ran > {}'}}\n",
            marker.display()
        ),
    )
    .expect("write adopted operation");
    support::sign_manifest(&operator_manifest);
    let wrapper = home.path().join("credential-wrapper.sh");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s' '{{\"labels\":[],\"files\":[{{\"path\":\"ostrom.yaml\"}}],\"title\":\"feat: grant\",\"baseRefOid\":\"{base_sha}\"}}'\n"
        ),
    )
    .expect("write metadata wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))
        .expect("make wrapper executable");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .current_dir(repository.path())
        .env("OSTROM_HOME", home.path())
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted_keys)
        .env("OSTROM_ACTOR", "builder")
        .env("MANDATE_GH_AS_BIN", wrapper)
        .args(["local-proof", "placeholder-org/repo#7"])
        .output()
        .expect("authorize operation from PR base");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not authorized"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !marker.exists(),
        "head-only grant must not govern its own pull request"
    );
}

#[test]
fn action_names_are_not_a_direct_cli_surface() {
    let root = fixture(LOCAL_POLICY);
    let output = ostrom(root.path())
        .args(["cmd/run", "placeholder-org/repo"])
        .output()
        .expect("run operation");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown operation `cmd/run`"));
}

#[test]
fn operations_list_and_settings_follow_grants() {
    let root = fixture(LOCAL_POLICY);
    let listing = ostrom(root.path())
        .args(["operations", "--actor", "builder"])
        .output()
        .expect("list operations");
    assert!(listing.status.success());
    assert_eq!(listing.stdout, b"local-proof\tLocal proof\n");

    let settings = ostrom(root.path())
        .args(["operations", "--settings", "builder"])
        .output()
        .expect("generate settings");
    assert!(settings.status.success());
    let settings = String::from_utf8(settings.stdout).expect("settings are UTF-8");
    assert!(settings.contains("\"defaultMode\": \"deny\""));
    assert!(settings.contains("Bash(ostrom local-proof *)"));
    assert!(!settings.contains("gatekeeper"));
}

#[test]
fn operations_prompt_resolves_inline_file_named_actor_and_loop_forms() {
    let cases = [
        ("'inline instructions'", "", "inline instructions"),
        ("{from: ./prompts/inspect.md}", "", "file instructions\n"),
        (
            "prompts.shared-inspection",
            "prompts:\n  shared-inspection: named instructions\n",
            "named instructions",
        ),
    ];
    for (prompt, declarations, expected) in cases {
        let root = fixture(&prompt_policy(prompt, declarations));
        if prompt.contains("from:") {
            fs::create_dir(root.path().join("prompts")).expect("create prompt directory");
            fs::write(root.path().join("prompts/inspect.md"), expected).expect("write prompt file");
        }
        for target in ["inspect", "builder", "inspection-loop"] {
            let output = ostrom(root.path())
                .args(["operations", "--prompt", target])
                .output()
                .expect("inspect resolved prompt");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, expected.as_bytes());
        }
    }
}

#[test]
fn tampered_prompt_file_fails_signature_verification() {
    let root = fixture(&prompt_policy("{from: prompts/inspect.md}", ""));
    fs::create_dir(root.path().join("prompts")).expect("create prompt directory");
    let prompt = root.path().join("prompts/inspect.md");
    fs::write(&prompt, "signed instructions\n").expect("write signed prompt");
    let manifest = root.path().join("policy.yaml");
    let trusted_keys = support::sign_manifest(&manifest);
    fs::write(&prompt, "tampered instructions\n").expect("tamper with prompt");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .env("OSTROM_HOME", root.path())
        .env("OSTROM_POLICY_MANIFEST", &manifest)
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted_keys)
        .args(["operations", "--prompt", "inspect"])
        .output()
        .expect("inspect tampered prompt");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("signature verification failed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn agent_runner_receives_resolved_prompt_text() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = fixture(&prompt_policy(
        "prompts.shared-inspection",
        "prompts:\n  shared-inspection: {from: prompts/inspect.md}\n",
    ));
    fs::create_dir(root.path().join("prompts")).expect("create prompt directory");
    fs::write(
        root.path().join("prompts/inspect.md"),
        "portable agent instructions\n",
    )
    .expect("write prompt");
    let capture = root.path().join("received-prompt");
    let runner = root.path().join("claude-stub");
    fs::write(
        &runner,
        "#!/bin/sh\nfor argument do last=$argument; done\nprintf '%s' \"$last\" >\"$OSTROM_CAPTURE\"\n",
    )
    .expect("write Claude stub");
    fs::set_permissions(&runner, fs::Permissions::from_mode(0o700))
        .expect("make Claude stub executable");

    let output = ostrom(root.path())
        .env("CLAUDE_BIN", runner)
        .env("OSTROM_CAPTURE", &capture)
        .args(["inspect", "placeholder-org/repo"])
        .output()
        .expect("run agent operation");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(capture).expect("read captured prompt"),
        "portable agent instructions\n"
    );
}

#[test]
fn settings_check_detects_a_hand_edit() {
    let root = fixture(LOCAL_POLICY);
    let generated = ostrom(root.path())
        .args(["operations", "--settings", "builder"])
        .output()
        .expect("generate settings");
    let settings = root.path().join("builder.settings.json");
    fs::write(
        &settings,
        String::from_utf8(generated.stdout)
            .expect("settings")
            .replace("local-proof", "hand-edit"),
    )
    .expect("write changed settings");
    let checked = ostrom(root.path())
        .arg("operations")
        .args(["--actor", "builder", "--check-settings"])
        .arg(settings)
        .output()
        .expect("check settings");
    assert!(!checked.status.success());
    assert!(String::from_utf8_lossy(&checked.stderr).contains("differs from settings derived"));
}

#[cfg(unix)]
#[test]
fn mediated_action_receives_only_its_catalogued_scope() {
    use std::os::unix::fs::PermissionsExt as _;

    let policy = "manifest_version: 1\nactors: {builder: {}}\noperations:\n  comment:\n    steps:\n      - uses: gh/post-verdict\n        with: {note: 'placeholder verdict'}\ngrants:\n  builder-comment: {actors: builder, operations: comment, repositories: placeholder-org/repo}\n";
    let root = fixture(policy);
    let capture = root.path().join("arguments.txt");
    let wrapper = root.path().join("credential-wrapper.sh");
    fs::write(
        &wrapper,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$OSTROM_CAPTURE\"\n",
    )
    .expect("write wrapper");
    let mut permissions = fs::metadata(&wrapper)
        .expect("wrapper metadata")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&wrapper, permissions).expect("make wrapper executable");

    let output = ostrom(root.path())
        .env("MANDATE_GH_AS_BIN", wrapper)
        .env("OSTROM_CAPTURE", &capture)
        .args(["comment", "placeholder-org/repo#7"])
        .output()
        .expect("run mediated operation");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let arguments = fs::read_to_string(capture).expect("captured arguments");
    assert_eq!(
        arguments,
        "builder\nplaceholder-org/repo\n--repositories\nplaceholder-org/repo\n--permissions\npull_requests:write\n--\ngh\npr\ncomment\nplaceholder-org/repo#7\n--body\nplaceholder verdict\n"
    );
}

#[cfg(unix)]
#[test]
fn failed_guard_stops_before_the_mediated_action() {
    use std::os::unix::fs::PermissionsExt as _;

    let policy = "manifest_version: 1\nincludes: [checks.yaml]\nactors: {gatekeeper: {}}\noperations:\n  merge:\n    steps:\n      - uses: gh/merge-pr\n        requires: ready\ngrants:\n  gatekeeper-merge: {actors: gatekeeper, operations: merge, repositories: placeholder-org/repo}\n";
    let root = fixture(policy);
    fs::write(
        root.path().join("checks.yaml"),
        "check: ready\nuses: cmd/run\nwith: {script: 'exit 1'}\n",
    )
    .expect("write checks");
    let capture = root.path().join("action-ran.txt");
    let wrapper = root.path().join("credential-wrapper.sh");
    fs::write(
        &wrapper,
        "#!/bin/sh\nprintf action-ran > \"$OSTROM_CAPTURE\"\n",
    )
    .expect("write wrapper");
    let mut permissions = fs::metadata(&wrapper)
        .expect("wrapper metadata")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&wrapper, permissions).expect("make wrapper executable");

    let output = ostrom(root.path())
        .env("OSTROM_ACTOR", "gatekeeper")
        .env("MANDATE_GH_AS_BIN", wrapper)
        .env("OSTROM_CAPTURE", &capture)
        .args(["merge", "placeholder-org/repo#7"])
        .output()
        .expect("run guarded operation");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("required check `ready` did not pass")
    );
    assert!(
        !capture.exists(),
        "guard must stop before token mint/action"
    );
}

#[cfg(unix)]
#[test]
fn selector_metadata_is_resolved_before_authorization() {
    use std::os::unix::fs::PermissionsExt as _;

    let policy = "manifest_version: 1\nactors: {builder: {}}\noperations:\n  comment:\n    steps:\n      - uses: gh/post-verdict\n        with: {note: 'placeholder verdict'}\ngrants:\n  approved-comment:\n    actors: builder\n    operations: comment\n    repositories: placeholder-org/repo\n    where: label:approved\n";
    let root = fixture(policy);
    let capture = root.path().join("action-ran.txt");
    let wrapper = root.path().join("credential-wrapper.sh");
    fs::write(
        &wrapper,
        "#!/bin/sh\ncase \" $* \" in\n  *\" pr view \"*) printf '%s' '{\"labels\":[{\"name\":\"approved\"}],\"files\":[{\"path\":\"src/lib.rs\"}],\"title\":\"feat: placeholder\"}' ;;\n  *) printf action-ran > \"$OSTROM_CAPTURE\" ;;\nesac\n",
    )
    .expect("write wrapper");
    let mut permissions = fs::metadata(&wrapper)
        .expect("wrapper metadata")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&wrapper, permissions).expect("make wrapper executable");

    let output = ostrom(root.path())
        .env("MANDATE_GH_AS_BIN", wrapper)
        .env("OSTROM_CAPTURE", &capture)
        .args(["comment", "placeholder-org/repo#7"])
        .output()
        .expect("run selector-constrained operation");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(capture).expect("action capture"),
        "action-ran"
    );
}
