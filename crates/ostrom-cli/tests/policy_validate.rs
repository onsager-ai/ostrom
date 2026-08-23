use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

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

fn normalized_manifest(working_directory: &Path, manifest: &Path, trusted_keys: &Path) -> Vec<u8> {
    let output = ostrom()
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted_keys)
        .current_dir(working_directory)
        .args(["validate", "--normalized"])
        .arg(manifest)
        .output()
        .expect("run normalized validate");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn repository_actor_portability_lint_reports_every_actor_origin_and_exits_zero() {
    let temporary = TempDir::new().expect("temporary repository");
    fs::create_dir(temporary.path().join(".git")).expect("repository boundary");
    let manifest = temporary.path().join("ostrom.yaml");
    let included = temporary.path().join("included-actor.yaml");
    fs::write(
        &manifest,
        "manifest_version: 1\nincludes: [included-actor.yaml]\nactors: {builder: {}}\n",
    )
    .expect("write repository manifest");
    fs::write(&included, "actor: gatekeeper\n").expect("write included actor");
    let trusted_keys = support::sign_manifest(&manifest);

    let output = ostrom()
        .env("OSTROM_HOME", temporary.path().join("operator-home"))
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted_keys)
        .args(["validate"])
        .arg(&manifest)
        .output()
        .expect("validate repository manifest");

    assert!(
        output.status.success(),
        "lint must report without refusing: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 findings");
    assert!(stderr.contains("repository actor `builder`"), "{stderr}");
    assert!(
        stderr.contains(manifest.to_str().expect("UTF-8 path")),
        "{stderr}"
    );
    assert!(stderr.contains("repository actor `gatekeeper`"), "{stderr}");
    assert!(
        stderr.contains(included.to_str().expect("UTF-8 path")),
        "{stderr}"
    );
}

#[test]
fn operator_actor_declarations_have_no_portability_findings() {
    let home = TempDir::new().expect("temporary operator home");
    let manifest = home.path().join("ostrom.yaml");
    fs::write(
        &manifest,
        "manifest_version: 1\nactors: {builder: {}, gatekeeper: {}}\n",
    )
    .expect("write operator manifest");
    let trusted_keys = support::sign_manifest(&manifest);

    let output = ostrom()
        .env("OSTROM_HOME", home.path())
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted_keys)
        .args(["validate"])
        .arg(&manifest)
        .output()
        .expect("validate operator manifest");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostics");
    assert!(!stderr.contains("non-portable"), "{stderr}");
    assert!(!stderr.contains("repository actor"), "{stderr}");
}

#[test]
fn glob_includes_compose_identically_for_all_manifest_path_forms() {
    let (root, manifest) = fixture();
    let trusted_keys =
        support::sign_manifest_from(&manifest, Path::new("manifest.yml"), root.path());

    let bare = normalized_manifest(root.path(), Path::new("manifest.yml"), &trusted_keys);
    let explicit = normalized_manifest(root.path(), Path::new("./manifest.yml"), &trusted_keys);
    let absolute = normalized_manifest(root.path(), &manifest, &trusted_keys);

    assert_eq!(bare, explicit);
    assert_eq!(bare, absolute);
}

#[test]
fn a_glob_whose_directory_is_missing_is_refused_like_any_other_empty_glob() {
    // Depth must not decide this. `gone/*.yml` and `gone/deeper/*.yml` describe
    // the same missing directory, and both are far more likely to be a typo than
    // an intentionally absent tree — a manifest that silently composes zero
    // includes is a manifest with no governance in it.
    for pattern in ["gone/*.yml", "gone/deeper/*.yml"] {
        let temporary = TempDir::new().expect("temporary directory");
        fs::write(
            temporary.path().join("manifest.yml"),
            format!("manifest_version: 1\nincludes: [{pattern}]\nactors: {{builder: {{}}}}\n"),
        )
        .expect("write manifest");

        let output = ostrom()
            .current_dir(temporary.path())
            .args(["sign", "--key-id", "unused", "--key", "unused.pem"])
            .arg("manifest.yml")
            .output()
            .expect("run policy signer");
        assert!(!output.status.success(), "{pattern} must not load");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("matched no files"),
            "{pattern}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn a_glob_with_an_existing_base_and_no_matches_is_refused() {
    let temporary = TempDir::new().expect("temporary directory");
    fs::create_dir(temporary.path().join("optional")).expect("create optional directory");
    fs::write(
        temporary.path().join("manifest.yml"),
        "manifest_version: 1\nincludes: [optional/*.yml]\n",
    )
    .expect("write manifest");

    let output = ostrom()
        .current_dir(temporary.path())
        .args(["sign", "--key-id", "unused", "--key", "unused.pem"])
        .arg("manifest.yml")
        .output()
        .expect("run policy signer");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("matched no files"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn non_glob_includes_compose_identically_for_all_manifest_path_forms() {
    let temporary = TempDir::new().expect("temporary directory");
    let manifest = temporary.path().join("manifest.yml");
    fs::write(&manifest, "manifest_version: 1\nincludes: [builder.yml]\n").expect("write manifest");
    fs::write(
        temporary.path().join("builder.yml"),
        "actor: builder\nname: Placeholder builder\n",
    )
    .expect("write included actor");
    let trusted_keys = support::sign_manifest(&manifest);

    let bare = normalized_manifest(temporary.path(), Path::new("manifest.yml"), &trusted_keys);
    let explicit =
        normalized_manifest(temporary.path(), Path::new("./manifest.yml"), &trusted_keys);
    let absolute = normalized_manifest(temporary.path(), &manifest, &trusted_keys);

    assert_eq!(bare, explicit);
    assert_eq!(bare, absolute);
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
    let (root, manifest) = fixture();
    let output = signed_ostrom(&manifest)
        .current_dir(root.path())
        .args(["validate", "--normalized"])
        .arg("manifest.yml")
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
        "check: available-placeholder-check\nuses: cmd/run\nwith: {script: 'exit 0'}\n",
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
    assert!(stderr.contains("add its file to `includes:`"), "{stderr}");
}

#[test]
fn require_resolves_an_included_check_by_exact_name() {
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
        "check: available-placeholder-check\nuses: cmd/run\nwith: {script: 'exit 0'}\n",
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
fn changing_an_included_check_script_after_signing_is_refused() {
    let temporary = TempDir::new().expect("temporary directory");
    let manifest = temporary.path().join("manifest.yml");
    let checks = temporary.path().join("checks.yaml");
    fs::write(
        &manifest,
        "manifest_version: 1\nincludes: [checks.yaml]\noperations:\n  merge:\n    steps:\n      - uses: gh/merge-pr\n        requires: ready-to-merge\n",
    )
    .expect("write root manifest");
    fs::write(
        &checks,
        "check: ready-to-merge\nuses: cmd/run\nwith: {script: 'exit 0'}\n",
    )
    .expect("write check leaf");
    let trusted = support::sign_manifest(&manifest);
    fs::write(
        &checks,
        "check: ready-to-merge\nuses: cmd/run\nwith: {script: 'exit 1'}\n",
    )
    .expect("change check script after signing");

    let output = ostrom()
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted)
        .args(["validate"])
        .arg(manifest)
        .output()
        .expect("validate changed check leaf");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("verification failed"));
}

#[test]
fn changing_an_inline_check_action_after_signing_is_refused() {
    let temporary = TempDir::new().expect("temporary directory");
    let manifest = temporary.path().join("manifest.yml");
    fs::write(
        &manifest,
        "manifest_version: 1\nchecks:\n  ready-to-merge:\n    uses: cmd/run\n    with: {script: 'exit 0'}\n",
    )
    .expect("write inline check");
    let trusted = support::sign_manifest(&manifest);
    fs::write(
        &manifest,
        "manifest_version: 1\nchecks:\n  ready-to-merge:\n    uses: gh/check-run\n    with: {name: rust}\n",
    )
    .expect("change check action after signing");

    let output = ostrom()
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted)
        .args(["validate"])
        .arg(manifest)
        .output()
        .expect("validate changed inline check");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("verification failed"));
}

#[test]
fn inline_and_leaf_checks_compose_and_sign_identically() {
    let inline_root = TempDir::new().expect("temporary inline directory");
    let inline_manifest = inline_root.path().join("manifest.yml");
    fs::write(
        &inline_manifest,
        "manifest_version: 1\ndefaults:\n  check: {inconclusive_policy: warn}\nchecks:\n  ready-to-merge:\n    uses: gh/check-run\n    with: {required: [rust]}\n    inconclusive_policy: block\n",
    )
    .expect("write inline manifest");

    let leaf_root = TempDir::new().expect("temporary leaf directory");
    let leaf_manifest = leaf_root.path().join("manifest.yml");
    fs::write(
        &leaf_manifest,
        "manifest_version: 1\nincludes: [checks.yaml]\ndefaults:\n  check: {inconclusive_policy: warn}\n",
    )
    .expect("write leaf manifest");
    fs::write(
        leaf_root.path().join("checks.yaml"),
        "check: ready-to-merge\nuses: gh/check-run\nwith: {required: [rust]}\ninconclusive_policy: block\n",
    )
    .expect("write check leaf");

    let inline_trusted = support::sign_manifest(&inline_manifest);
    let leaf_trusted = support::sign_manifest(&leaf_manifest);
    let inline_normalized = ostrom()
        .env("OSTROM_POLICY_TRUSTED_KEYS", inline_trusted)
        .args(["validate", "--normalized"])
        .arg(&inline_manifest)
        .output()
        .expect("normalize inline manifest");
    let leaf_normalized = ostrom()
        .env("OSTROM_POLICY_TRUSTED_KEYS", leaf_trusted)
        .args(["validate", "--normalized"])
        .arg(&leaf_manifest)
        .output()
        .expect("normalize leaf manifest");
    assert!(inline_normalized.status.success());
    assert!(leaf_normalized.status.success());
    assert_eq!(inline_normalized.stdout, leaf_normalized.stdout);
    assert_eq!(
        fs::read(inline_manifest.with_extension("yml.sig")).expect("read inline signature"),
        fs::read(leaf_manifest.with_extension("yml.sig")).expect("read leaf signature")
    );
}

#[test]
fn a_check_leaf_with_another_identity_key_is_refused() {
    let temporary = TempDir::new().expect("temporary directory");
    let manifest = temporary.path().join("manifest.yml");
    fs::write(&manifest, "manifest_version: 1\nincludes: [checks.yaml]\n")
        .expect("write root manifest");
    fs::write(
        temporary.path().join("checks.yaml"),
        "check: ready-to-merge\nactor: builder\nuses: cmd/run\nwith: {script: 'exit 0'}\n",
    )
    .expect("write ambiguous leaf");

    let output = ostrom()
        .args(["validate"])
        .arg(manifest)
        .output()
        .expect("validate ambiguous leaf");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("multiple identity keys")
            && stderr.contains("actor")
            && stderr.contains("check"),
        "{stderr}"
    );
}

#[test]
fn a_manifest_without_checks_still_signs_and_verifies() {
    let temporary = TempDir::new().expect("temporary directory");
    let manifest = temporary.path().join("manifest.yml");
    fs::write(&manifest, "manifest_version: 1\nactors: {builder: {}}\n").expect("write manifest");
    let trusted = support::sign_manifest(&manifest);

    let output = ostrom()
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted)
        .args(["validate"])
        .arg(manifest)
        .output()
        .expect("validate manifest without checks");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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
