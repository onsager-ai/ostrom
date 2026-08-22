use std::{fs, path::PathBuf, process::Command};

use ostrom_core::{PolicyCandidate, PolicyManifest};
use tempfile::TempDir;

mod support;

fn fixture() -> (TempDir, PathBuf) {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/policy");
    let root = support::copy_fixture_directory(&source);
    let manifest = root.path().join("manifest.yml");
    (root, manifest)
}

fn ostrom() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
    command.env_remove("OSTROM_FIXTURE_CADENCE");
    command.env_remove("OSTROM_FIXTURE_TOKEN");
    command
}

fn signed_ostrom(manifest: &std::path::Path) -> Command {
    let mut command = ostrom();
    command.env(
        "OSTROM_POLICY_TRUSTED_KEYS",
        support::sign_manifest(manifest),
    );
    command
}

#[test]
fn validate_and_normalized_accept_the_composed_fixture() {
    let (_root, manifest) = fixture();
    let plain = signed_ostrom(&manifest)
        .args(["validate"])
        .arg(&manifest)
        .output()
        .expect("run validate");
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    assert!(String::from_utf8_lossy(&plain.stdout).starts_with("valid: "));

    let normalized = signed_ostrom(&manifest)
        .args(["validate", "--normalized"])
        .arg(&manifest)
        .output()
        .expect("run normalized validate");
    assert!(
        normalized.status.success(),
        "{}",
        String::from_utf8_lossy(&normalized.stderr)
    );
    let yaml = String::from_utf8(normalized.stdout).expect("normalized YAML is UTF-8");
    assert!(!yaml.contains("includes:"));
    assert!(yaml.contains("actors:\n    - builder"), "{yaml}");
    PolicyManifest::from_yaml(&yaml).expect("normalized output remains a valid manifest");
}

#[test]
fn fixture_reproduces_builder_decisions_for_all_eleven_repositories() {
    let (_root, manifest) = fixture();
    let output = signed_ostrom(&manifest)
        .args(["validate", "--normalized"])
        .arg(manifest)
        .output()
        .expect("run normalized validate");
    assert!(output.status.success());
    let manifest = PolicyManifest::from_yaml(
        &String::from_utf8(output.stdout).expect("normalized YAML is UTF-8"),
    )
    .expect("normalized manifest parses");

    let cases = [
        ("onsager-ai/duhem", Some("area:schema"), None),
        ("onsager-ai/duhem-hub", Some("area:ingest"), None),
        ("onsager-ai/duhem-site", None, Some("feat")),
        ("onsager-ai/chreode", Some("area:platform"), None),
        ("onsager-ai/kirzner", Some("area:ux"), None),
        ("crawlab-team/crawlab-pro", Some("ui"), None),
        ("onsager-ai/ostrom", None, Some("docs")),
        ("onsager-ai/ostrom-hub", Some("area:data"), None),
        ("onsager-ai/dev-skills", Some("bug"), None),
        ("onsager-ai/semon", None, None),
        ("onsager-ai/onsager", Some("area:infra"), None),
    ];
    for (repository, label, commit_type) in cases {
        let candidate = PolicyCandidate {
            repository: repository.to_owned(),
            labels: label.into_iter().map(str::to_owned).collect(),
            commit_type: commit_type.map(str::to_owned),
            ..PolicyCandidate::default()
        };
        assert!(
            manifest.decide("builder", "work", &candidate).granted,
            "{repository} should resolve to the builder"
        );
    }
}

#[test]
fn duhem_area_schema_replay_resolves_to_builder() {
    let (_root, manifest) = fixture();
    let output = signed_ostrom(&manifest)
        .args(["validate", "--normalized"])
        .arg(manifest)
        .output()
        .expect("run normalized validate");
    let manifest = PolicyManifest::from_yaml(
        &String::from_utf8(output.stdout).expect("normalized YAML is UTF-8"),
    )
    .expect("normalized manifest parses");
    let candidate = PolicyCandidate {
        repository: "onsager-ai/duhem".to_owned(),
        labels: vec!["area:schema".to_owned()],
        ..PolicyCandidate::default()
    };
    assert!(manifest.decide("builder", "work", &candidate).granted);
}

#[test]
fn duplicate_grant_across_includes_names_both_files() {
    let temporary = TempDir::new().expect("temporary directory");
    fs::write(
        temporary.path().join("manifest.yml"),
        "manifest_version: 1\nincludes: [first.yml, second.yml]\nactors: {builder: {}}\noperations: {work: {steps: []}}\n",
    )
    .expect("write root");
    for name in ["first.yml", "second.yml"] {
        fs::write(
            temporary.path().join(name),
            "grant: duplicate\nactors: builder\noperations: work\n",
        )
        .expect("write leaf");
    }
    let output = ostrom()
        .args(["validate"])
        .arg(temporary.path().join("manifest.yml"))
        .output()
        .expect("run validate");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("duplicate grant id `duplicate`"),
        "{stderr}"
    );
    assert!(stderr.contains("first.yml"), "{stderr}");
    assert!(stderr.contains("second.yml"), "{stderr}");
}

#[test]
fn deny_beats_grant_in_either_include_order() {
    let temporary = TempDir::new().expect("temporary directory");
    fs::write(
        temporary.path().join("grant.yml"),
        "grant: delegated\nactors: builder\noperations: work\nwhere: label:area:schema\n",
    )
    .expect("write grant");
    fs::write(
        temporary.path().join("deny.yml"),
        "deny: protected\nactors: builder\noperations: work\nwhere: label:area:schema\n",
    )
    .expect("write deny");
    for (root, includes) in [
        ("grant-first.yml", "[grant.yml, deny.yml]"),
        ("deny-first.yml", "[deny.yml, grant.yml]"),
    ] {
        let path = temporary.path().join(root);
        fs::write(
            &path,
            format!(
                "manifest_version: 1\nincludes: {includes}\nactors: {{builder: {{}}}}\noperations: {{work: {{steps: []}}}}\n"
            ),
        )
        .expect("write root");
        let output = signed_ostrom(&path)
            .args(["validate", "--normalized"])
            .arg(path)
            .output()
            .expect("run validate");
        assert!(output.status.success());
        let manifest = PolicyManifest::from_yaml(
            &String::from_utf8(output.stdout).expect("normalized YAML is UTF-8"),
        )
        .expect("normalized manifest parses");
        let candidate = PolicyCandidate {
            labels: vec!["area:schema".to_owned()],
            ..PolicyCandidate::default()
        };
        assert!(!manifest.decide("builder", "work", &candidate).granted);
    }
}

#[test]
fn unknown_selector_prefix_fails_the_cli_and_names_it() {
    let temporary = TempDir::new().expect("temporary directory");
    let manifest = temporary.path().join("manifest.yml");
    fs::write(
        &manifest,
        "manifest_version: 1\nactors: {builder: {}}\noperations: {work: {steps: []}}\ngrants:\n  bad: {actors: builder, operations: work, where: 'title:*free prose*'}\n",
    )
    .expect("write manifest");
    let output = ostrom()
        .args(["validate"])
        .arg(manifest)
        .output()
        .expect("run validate");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown selector prefix: title"),
        "{stderr}"
    );
}

#[test]
fn require_naming_an_undefined_check_fails_the_load() {
    let temporary = TempDir::new().expect("temporary directory");
    let manifest = temporary.path().join("manifest.yml");
    fs::write(
        &manifest,
        "manifest_version: 1\noperations:\n  merge-placeholder:\n    steps:\n      - uses: gh/merge-pr\n        requires: missing-placeholder-check\n",
    )
    .expect("write manifest");
    fs::write(
        temporary.path().join("checks.yaml"),
        "checks_version: 1\nchecks:\n  available-placeholder-check:\n    uses: cmd/run\n    with: {script: 'exit 0'}\n",
    )
    .expect("write checks");

    let output = ostrom()
        .args(["validate"])
        .arg(manifest)
        .output()
        .expect("run validate");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires undefined check `missing-placeholder-check`"),
        "{stderr}"
    );
}

#[test]
fn require_resolves_a_sibling_check_by_exact_name() {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/checks-verdict");
    let root = support::copy_fixture_directory(&source);
    let fixture = root.path().join("manifest.yml");
    let output = signed_ostrom(&fixture)
        .args(["validate"])
        .arg(fixture)
        .output()
        .expect("run validate");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn operation_names_cannot_shadow_builtins_or_actions() {
    for name in ["operations", "credential", "gh/merge-pr"] {
        let temporary = TempDir::new().expect("temporary directory");
        let manifest = temporary.path().join("manifest.yml");
        fs::write(
            &manifest,
            format!("manifest_version: 1\noperations:\n  {name}: {{steps: []}}\n"),
        )
        .expect("write manifest");
        let output = ostrom()
            .args(["validate"])
            .arg(manifest)
            .output()
            .expect("run validate");
        assert!(!output.status.success(), "{name} must fail");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("operation name") && stderr.contains(name),
            "{stderr}"
        );
    }
}

#[test]
fn grant_requires_naming_an_undefined_check_fails_the_load() {
    let temporary = TempDir::new().expect("temporary directory");
    let manifest = temporary.path().join("manifest.yml");
    fs::write(
        &manifest,
        "manifest_version: 1\ngrants:\n  placeholder-grant:\n    requires: missing-placeholder-check\n",
    )
    .expect("write manifest");
    fs::write(
        temporary.path().join("checks.yaml"),
        "checks_version: 1\nchecks:\n  available-placeholder-check:\n    uses: cmd/run\n    with: {script: 'exit 0'}\n",
    )
    .expect("write checks");

    let output = ostrom()
        .args(["validate"])
        .arg(manifest)
        .output()
        .expect("run validate");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("grant `placeholder-grant`"), "{stderr}");
    assert!(
        stderr.contains("requires undefined check `missing-placeholder-check`"),
        "{stderr}"
    );
}

#[test]
fn unsigned_manifest_is_refused() {
    let temporary = TempDir::new().expect("temporary directory");
    let manifest = temporary.path().join("manifest.yml");
    let trusted = temporary.path().join("trusted");
    fs::create_dir(&trusted).expect("create empty trusted key directory");
    fs::write(&manifest, "manifest_version: 1\nloops: {}\n").expect("write manifest");

    let output = ostrom()
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted)
        .args(["validate"])
        .arg(manifest)
        .output()
        .expect("validate unsigned manifest");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("policy signature is missing"));
}

#[test]
fn changing_the_root_after_signing_is_refused() {
    let temporary = TempDir::new().expect("temporary directory");
    let manifest = temporary.path().join("manifest.yml");
    fs::write(
        &manifest,
        "manifest_version: 1\nactors: {builder: {name: Placeholder builder}}\n",
    )
    .expect("write manifest");
    let trusted = support::sign_manifest(&manifest);
    fs::write(
        &manifest,
        "manifest_version: 1\nactors: {builder: {name: Changed placeholder}}\n",
    )
    .expect("change manifest after signing");

    let output = ostrom()
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted)
        .args(["validate"])
        .arg(manifest)
        .output()
        .expect("validate changed manifest");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("verification failed"));
}

#[test]
fn signature_from_an_untrusted_key_is_refused() {
    let temporary = TempDir::new().expect("temporary directory");
    let manifest = temporary.path().join("manifest.yml");
    fs::write(&manifest, "manifest_version: 1\n").expect("write manifest");
    support::sign_manifest(&manifest);
    let unrelated_trust_set = temporary.path().join("unrelated-trusted-keys");
    fs::create_dir(&unrelated_trust_set).expect("create unrelated trust set");

    let output = ostrom()
        .env("OSTROM_POLICY_TRUSTED_KEYS", unrelated_trust_set)
        .args(["validate"])
        .arg(manifest)
        .output()
        .expect("validate with unrelated trust set");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("untrusted key"));
}

#[test]
fn changing_an_included_leaf_after_signing_is_refused() {
    let temporary = TempDir::new().expect("temporary directory");
    let manifest = temporary.path().join("manifest.yml");
    let leaf = temporary.path().join("builder.yml");
    fs::write(&manifest, "manifest_version: 1\nincludes: [builder.yml]\n")
        .expect("write root manifest");
    fs::write(&leaf, "actor: builder\nname: Placeholder builder\n").expect("write actor leaf");
    let trusted = support::sign_manifest(&manifest);
    fs::write(&leaf, "actor: builder\nname: Changed placeholder\n")
        .expect("change included leaf after signing");

    let output = ostrom()
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted)
        .args(["validate"])
        .arg(manifest)
        .output()
        .expect("validate changed included leaf");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("verification failed"));
}

#[test]
fn loop_substrate_receives_no_private_signing_key() {
    let temporary = TempDir::new().expect("temporary loop substrate");
    let manifest = temporary.path().join("manifest.yml");
    fs::write(&manifest, "manifest_version: 1\nloops: {}\n").expect("write manifest");
    let trusted = support::sign_manifest(&manifest);

    let substrate_files = fs::read_dir(temporary.path())
        .expect("read loop substrate")
        .map(|entry| entry.expect("read substrate entry").file_name())
        .collect::<Vec<_>>();
    assert!(manifest.with_extension("yml.sig").is_file());
    assert!(trusted.join("placeholder-principal.pem").is_file());
    assert!(
        substrate_files
            .iter()
            .all(|name| !name.to_string_lossy().contains("private"))
    );
}
