use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use tempfile::{TempDir, tempdir};

mod support;

fn ostrom() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ostrom"))
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/loops")
}

/// A signed copy of the loop fixture. `ostrom` refuses an unsigned manifest,
/// and the signature must not be written beside the checked-in fixture, so the
/// tree is copied into a temporary directory and signed there.
struct SignedFixture {
    _root: TempDir,
    manifest: PathBuf,
    trusted_keys: PathBuf,
}

impl SignedFixture {
    fn new() -> Self {
        let root = support::copy_fixture_directory(&fixture());
        let manifest = root.path().join("policy.yaml");
        let trusted_keys = support::sign_manifest(&manifest);
        Self {
            _root: root,
            manifest,
            trusted_keys,
        }
    }

    fn ostrom(&self) -> Command {
        let mut command = ostrom();
        command
            .env("OSTROM_POLICY_MANIFEST", &self.manifest)
            .env("OSTROM_POLICY_TRUSTED_KEYS", &self.trusted_keys);
        command
    }
}

#[test]
fn rendered_units_match_the_committed_fixture_and_check_clean() {
    let root = tempdir().expect("temporary render fixture");
    let policy = SignedFixture::new();
    let output = policy
        .ostrom()
        .args(["loops", "render", "--output"])
        .arg(root.path())
        .env("OSTROM_HOME", root.path())
        .output()
        .expect("render loop units");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let expected = fixture().join("expected");
    let expected_names = unit_names(&expected);
    assert_eq!(unit_names(root.path()), expected_names);
    for name in expected_names {
        assert_eq!(
            fs::read(root.path().join(&name)).expect("rendered unit"),
            fs::read(expected.join(&name)).expect("expected unit"),
            "{name}"
        );
    }

    let checked = policy
        .ostrom()
        .args(["loops", "check"])
        .arg(root.path())
        .env("OSTROM_HOME", root.path())
        .output()
        .expect("check rendered units");
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
}

// ostrom#599 / #612: the sweep-lease wait ceiling is bounded by this same
// `TimeoutStartSec`, a value `umwelt-runtime`'s loop unit renderer holds as a
// template literal rather than a constant it shares with `ostrom-store`. This
// is the independent side of that relationship (repo principle 6): if the
// rendered unit's timeout moves, or the ceiling moves, this fails until
// `ostrom_store::SWEEP_LEASE_CEILING_SECONDS` and
// `ostrom_store::MINIMUM_PASS_WORK_SECONDS` still leave a valid gap.
//
// This must never be equality. `TimeoutStartSec` is when the supervisor
// SIGTERMs the unit (`Type=oneshot`, `KillMode=control-group`), not a budget
// the wait gets to spend — a ceiling equal to or close to it means a pass
// waiting out a lease that never frees hits the wait expiry and the kill at
// the same instant, so contention can never be observed and reported before
// the unit is killed out from under it (#612).
#[test]
fn every_loop_services_timeout_agrees_with_the_sweep_lease_ceiling() {
    let expected = fixture().join("expected");
    // `ostrom-up.service` is excluded deliberately, not by oversight: it runs
    // `ostrom up` to reconcile loops from policy, not a pass, so it is a
    // `Type=oneshot` unit rendered without a `TimeoutStartSec` line at all
    // (verified against the checked-in fixture) — including it would panic
    // below on "no TimeoutStartSec line" rather than test anything about the
    // sweep lease ceiling.
    let mut services = unit_names(&expected)
        .into_iter()
        .filter(|name| name.ends_with(".service") && name != "ostrom-up.service")
        .peekable();
    assert!(services.peek().is_some(), "no loop .service fixtures found");
    for name in services {
        let contents = fs::read_to_string(expected.join(&name)).expect("expected unit contents");
        let timeout_line = contents
            .lines()
            .find(|line| line.starts_with("TimeoutStartSec="))
            .unwrap_or_else(|| panic!("{name} has no TimeoutStartSec line"));
        let seconds: u64 = timeout_line
            .trim_start_matches("TimeoutStartSec=")
            .parse()
            .unwrap_or_else(|error| panic!("{name}: malformed TimeoutStartSec: {error}"));
        // Catches raising the ceiling toward (or past) the unit's kill
        // deadline: a wait that can no longer time out before the SIGTERM
        // would never surface as observable contention.
        assert!(
            seconds > ostrom_store::SWEEP_LEASE_CEILING_SECONDS,
            "{name}'s TimeoutStartSec ({seconds}s) no longer leaves the sweep lease ceiling \
             ({}s) room to time out before the unit is killed",
            ostrom_store::SWEEP_LEASE_CEILING_SECONDS
        );
        // Catches lowering the unit's own timeout (independently of the
        // ceiling): even with the ceiling unchanged, too small a gap leaves
        // no room for the sweep and agent turn the wait exists to let run.
        assert!(
            seconds - ostrom_store::SWEEP_LEASE_CEILING_SECONDS
                >= ostrom_store::MINIMUM_PASS_WORK_SECONDS,
            "{name}'s TimeoutStartSec ({seconds}s) leaves less than \
             MINIMUM_PASS_WORK_SECONDS ({}s) after the sweep lease ceiling ({}s)",
            ostrom_store::MINIMUM_PASS_WORK_SECONDS,
            ostrom_store::SWEEP_LEASE_CEILING_SECONDS
        );
    }
}

#[test]
fn unattended_triage_has_its_own_actor_settings_profile() {
    let root = tempdir().expect("temporary settings fixture");
    let policy = SignedFixture::new();
    let output = policy
        .ostrom()
        .args(["operations", "--settings", "triage"])
        .env("OSTROM_HOME", root.path())
        .output()
        .expect("render triage settings");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let settings = String::from_utf8(output.stdout).expect("settings are UTF-8");
    assert!(settings.contains("\"OSTROM_ACTOR\": \"triage\""));
    assert!(settings.contains("Bash(ostrom queue-triage *)"));
    assert!(!settings.contains("Bash(ostrom build-pass *)"));
    assert!(!settings.contains("Bash(ostrom gate-pass *)"));
}

#[test]
fn drift_check_refuses_a_hand_edit_without_touching_it() {
    let root = tempdir().expect("temporary drift fixture");
    let unit = root.path().join("ostrom-loop-builder-day.timer");
    fs::write(&unit, "placeholder hand edit\n").expect("write hand edit");
    let before = fs::read(&unit).expect("read before");
    let policy = SignedFixture::new();
    let output = policy
        .ostrom()
        .args(["loops", "check"])
        .arg(root.path())
        .env("OSTROM_HOME", root.path())
        .output()
        .expect("check drift");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("changed: ostrom-loop-builder-day.timer"),
        "{stderr}"
    );
    assert_eq!(fs::read(unit).expect("read after"), before);
}

#[test]
fn loop_run_injects_declared_ceilings_and_refuses_an_enforced_mismatch() {
    let root = tempdir().expect("temporary loop run fixture");
    let manifest = root.path().join("policy.yaml");
    let marker = root.path().join("ceilings.txt");
    fs::write(
        &manifest,
        r#"manifest_version: 1
defaults:
  loop: {concurrent: 6, spend_usd: 50, tokens: 200000}
actors: {builder: {}}
operations:
  scheduled-work:
    steps:
      - uses: cmd/run
        with:
          script: 'printf "%s|%s|%s\n" "$MANDATE_DAILY_CAP_USD" "$MANDATE_MAX_IMPLEMENTERS" "$MANDATE_ORDER_TOKEN_CEILING" > "$OSTROM_LOOP_MARKER"'
grants:
  scheduled-work:
    actors: builder
    operations: scheduled-work
    repositories: placeholder-org/repository
loops:
  builder-night:
    actor: builder
    operation: scheduled-work
    repositories: placeholder-org/repository
    every: ["23:15", "02:15", "05:15"]
    concurrent: 2
"#,
    )
    .expect("write policy fixture");
    let trusted_keys = support::sign_manifest(&manifest);
    let operator = root.path().join("ostrom.yaml");
    fs::copy(&manifest, &operator).expect("install operator policy fixture");
    support::sign_manifest(&operator);
    let composed = ostrom()
        .arg("compose")
        .arg(&manifest)
        .env("OSTROM_HOME", root.path())
        .env("OSTROM_POLICY_MANIFEST", &operator)
        .env("OSTROM_POLICY_TRUSTED_KEYS", &trusted_keys)
        .output()
        .expect("compose loop policy");
    assert!(
        composed.status.success(),
        "{}",
        String::from_utf8_lossy(&composed.stderr)
    );

    let output = loop_run(root.path(), &manifest, &trusted_keys, &marker)
        .output()
        .expect("run loop");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&marker).expect("ceiling marker"),
        "50|2|200000\n"
    );

    fs::remove_file(&marker).expect("remove marker");
    let mismatch = loop_run(root.path(), &manifest, &trusted_keys, &marker)
        .env("MANDATE_MAX_IMPLEMENTERS", "9")
        .output()
        .expect("run mismatched loop");
    assert!(!mismatch.status.success());
    let stderr = String::from_utf8_lossy(&mismatch.stderr);
    assert!(
        stderr.contains("ceiling mismatch for `concurrent`"),
        "{stderr}"
    );
    assert!(!marker.exists(), "dispatch must stop before its operation");
}

#[test]
fn non_agent_loop_refuses_an_empty_effective_scope_with_terminal_records() {
    let root = tempdir().expect("temporary empty-scope loop fixture");
    let manifest = root.path().join("policy.yaml");
    let marker = root.path().join("operation-ran");
    fs::write(
        &manifest,
        r#"manifest_version: 1
actors: {triage: {}}
operations:
  local-triage:
    steps:
      - uses: cmd/run
        with:
          script: 'printf ran > "$OSTROM_LOOP_MARKER"'
grants:
  local-triage:
    actors: triage
    operations: local-triage
    repositories: placeholder-org/other
loops:
  unattended-triage:
    actor: triage
    operation: local-triage
    repositories: placeholder-org/unavailable
    every: hourly
"#,
    )
    .expect("write empty-scope policy");
    let trusted_keys = support::sign_manifest(&manifest);
    let operator = root.path().join("ostrom.yaml");
    fs::copy(&manifest, &operator).expect("install operator policy fixture");
    support::sign_manifest(&operator);
    let composed = ostrom()
        .arg("compose")
        .arg(&manifest)
        .env("OSTROM_HOME", root.path())
        .env("OSTROM_POLICY_MANIFEST", &operator)
        .env("OSTROM_POLICY_TRUSTED_KEYS", &trusted_keys)
        .output()
        .expect("compose empty-scope policy");
    assert!(
        composed.status.success(),
        "{}",
        String::from_utf8_lossy(&composed.stderr)
    );

    let output = ostrom()
        .args(["loop", "run", "unattended-triage"])
        .env("OSTROM_HOME", root.path())
        .env("OSTROM_POLICY_TRUSTED_KEYS", &trusted_keys)
        .env("OSTROM_AVAILABLE_REPOSITORIES", "placeholder-org/alpha")
        .env("OSTROM_LOOP_MARKER", &marker)
        .output()
        .expect("run empty-scope non-agent loop");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no-effective-repositories"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!marker.exists(), "empty-scope operation ran");
    let trace = fs::read_to_string(root.path().join("sprint.jsonl"))
        .expect("read empty-scope trace")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("trace row"))
        .collect::<Vec<_>>();
    assert_eq!(trace.len(), 2, "{trace:?}");
    assert_eq!(trace[0]["kind"], "pass-started");
    assert_eq!(trace[1]["kind"], "pass-ended");
    for row in &trace {
        assert_eq!(row["fact"]["repositories"], serde_json::json!([]));
        assert_eq!(
            row["fact"]["skipped_repositories"],
            serde_json::json!([{
                "repository": "placeholder-org/unavailable",
                "reason": "repository-not-available"
            }])
        );
        assert_eq!(row["fact"]["reason"], "no-effective-repositories");
    }
    assert_eq!(trace[1]["fact"]["outcome"], "failed");
}

fn loop_run(root: &Path, manifest: &Path, trusted_keys: &Path, marker: &Path) -> Command {
    let mut command = ostrom();
    command
        .args(["loop", "run", "builder-night"])
        .env("OSTROM_HOME", root)
        .env("OSTROM_POLICY_MANIFEST", manifest)
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted_keys)
        .env("OSTROM_LOOP_MARKER", marker)
        .env_remove("OSTROM_ACTOR")
        .env_remove("MANDATE_DAILY_CAP_USD")
        .env_remove("MANDATE_MAX_IMPLEMENTERS")
        .env_remove("MANDATE_ORDER_TOKEN_CEILING");
    command
}

fn unit_names(directory: &Path) -> Vec<String> {
    let mut names = fs::read_dir(directory)
        .expect("unit fixture directory")
        .map(|entry| {
            entry
                .expect("unit fixture entry")
                .file_name()
                .into_string()
                .expect("UTF-8 fixture name")
        })
        .filter(|name| name.ends_with(".service") || name.ends_with(".timer"))
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// Rendering writes unit files; it must never switch one on.
///
/// This is the boundary `dotclaude/systemd/enabled-timers` exists to hold. Its
/// header records why: before it, writing a unit file into that repository was
/// "sufficient to get arbitrary code running on a schedule as this user, within
/// 15 minutes, with nothing in between — and agents write files in this
/// repository."
///
/// `sys/enable-loop` is ungrantable, so no operation can confer enabling. That
/// covers the manifest. This covers the renderer: today it holds because the
/// render path simply contains no activation call, and a property that holds by
/// absence is one a later change removes without noticing.
#[test]
fn the_loop_renderer_never_activates_a_unit() {
    let source = include_str!("../src/main.rs");
    let start = source
        .find("fn run_loops_command")
        .expect("the loops command exists");
    let region = &source[start..];
    let end = region[1..]
        .find("\nfn ")
        .map_or(region.len(), |offset| offset + 1);
    let body = &region[..end];

    for forbidden in [
        "systemctl",
        "enable --now",
        "daemon-reload",
        "MANDATE_SYSTEMCTL_BIN",
    ] {
        assert!(
            !body.contains(forbidden),
            "the loop renderer must not reference `{forbidden}`: rendering a unit \
             and activating one are different authorities, and only the second is \
             the principal's"
        );
    }
}
