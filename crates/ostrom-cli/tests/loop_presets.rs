use std::{fs, path::Path, process::Command};

use ostrom_core::PolicyManifest;
use serde_json::Value;
use tempfile::TempDir;

mod support;

fn command(home: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .current_dir(home)
        .env("OSTROM_HOME", home)
        .env_remove("OSTROM_POLICY_TRUSTED_KEYS")
        .args(args)
        .output()
        .expect("run ostrom");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    String::from_utf8(output.stdout).expect("UTF-8 output")
}

fn presets(home: &Path) -> Value {
    serde_json::from_str(&command(home, &["loops", "presets", "--json"])).expect("presets JSON")
}

fn fill_placeholders(preset: &Value) -> PolicyManifest {
    let mut fragment = preset["fragment"].clone();
    for path in preset["placeholder_paths"].as_array().expect("paths") {
        let value = fragment
            .pointer_mut(path.as_str().expect("JSON Pointer"))
            .expect("placeholder exists");
        assert_eq!(value, "placeholder-org/portfolio");
        *value = Value::String("placeholder-org/adopted-repository".to_owned());
    }
    assert!(!fragment.to_string().contains("placeholder-org/portfolio"));
    serde_json::from_value(fragment).expect("manifest fragment")
}

fn compose(home: &Path) -> PolicyManifest {
    let manifest = home.join("ostrom.yaml");
    let trusted_keys = support::sign_manifest(&manifest);
    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .current_dir(home)
        .env("OSTROM_HOME", home)
        .env("OSTROM_POLICY_TRUSTED_KEYS", trusted_keys)
        .arg("compose")
        .arg(&manifest)
        .output()
        .expect("compose operator manifest");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let materialized =
        fs::read_to_string(home.join("current/ostrom.yaml")).expect("composed manifest");
    PolicyManifest::parse_yaml(&materialized).expect("parse composed manifest")
}

#[test]
fn formats_agree_and_have_stable_order_without_operator_configuration() {
    let home = TempDir::new().expect("temporary home");
    let yaml = command(home.path(), &["loops", "presets"]);
    let json = command(home.path(), &["loops", "presets", "--json"]);
    assert_eq!(yaml, command(home.path(), &["loops", "presets"]));
    assert_eq!(json, command(home.path(), &["loops", "presets", "--json"]));
    assert_eq!(fs::read_dir(home.path()).expect("home entries").count(), 0);
    let catalogue: Value = serde_json::from_str(&json).expect("JSON catalogue");
    let catalogue = catalogue.as_object().expect("catalogue object");
    assert_eq!(
        catalogue.keys().map(String::as_str).collect::<Vec<_>>(),
        ["builder", "gatekeeper", "plan", "sweep", "triage"]
    );
    assert_eq!(
        catalogue["builder"]["secret_names"],
        serde_json::json!(["builder"])
    );
    assert_eq!(
        catalogue["gatekeeper"]["secret_names"],
        serde_json::json!(["gatekeeper"])
    );
    assert_eq!(
        catalogue["plan"]["secret_names"],
        serde_json::json!(["gatekeeper"])
    );
    assert_eq!(
        catalogue["sweep"]["secret_names"],
        serde_json::json!(["gatekeeper"])
    );
    assert_eq!(
        catalogue["triage"]["secret_names"],
        serde_json::json!(["triage"])
    );
    let mut combined = PolicyManifest::parse_yaml("manifest_version: 1\n").unwrap();
    for (name, preset) in catalogue {
        let mut expected_keys = vec!["fragment", "secret_names", "placeholder_paths"];
        if name.as_str() == "sweep" {
            expected_keys.push("deprecated");
        }
        assert_eq!(
            preset
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            expected_keys,
            "{name}"
        );
        assert!(
            preset["secret_names"]
                .as_array()
                .unwrap()
                .iter()
                .all(Value::is_string)
        );
        let fragment: PolicyManifest =
            serde_json::from_value(preset["fragment"].clone()).expect("schema accepts fragment");
        combined.actors.extend(fragment.actors);
        combined.operations.extend(fragment.operations);
        combined.grants.extend(fragment.grants);
        combined.loops.extend(fragment.loops);
    }
    assert_eq!(combined, PolicyManifest::parse_yaml(&yaml).unwrap());
    let fixture = PolicyManifest::parse_yaml(include_str!("fixtures/loops/policy.yaml"))
        .expect("loop fixture");
    for (name, declaration) in &combined.loops {
        let expected = &fixture.loops[name];
        assert_eq!(declaration.actor, expected.actor);
        assert_eq!(declaration.operation, expected.operation);
        assert_eq!(declaration.repositories, expected.repositories);
        assert_eq!(declaration.every, expected.every);
    }
    assert_eq!(combined.loops.len(), 4);
    assert!(!combined.loops.contains_key("sweep"));
    assert_eq!(combined.loops["builder-day"].spend_usd, Some(20.0));
    assert_eq!(combined.loops["builder-day"].concurrent, Some(1));
    assert_eq!(combined.loops["daily-plan"].spend_usd, Some(5.0));
    assert_eq!(combined.loops["daily-plan"].concurrent, Some(1));
    assert_eq!(
        combined.loops["daily-plan"].every.on_calendars(),
        ["*-*-* 06:30:00"]
    );
    let sweep = &combined.operations["portfolio-sweep"];
    assert_eq!(sweep.steps.len(), 1);
    assert_eq!(sweep.steps[0].uses, "cmd/run");
    assert_eq!(sweep.steps[0].parameters["script"], "ostrom sweep");
    assert!(combined.actors.contains_key("sweeper"));
    assert!(combined.grants.contains_key("sweep"));
}

#[test]
fn only_the_sweep_preset_carries_a_deprecated_field_and_it_names_no_hosted_substrate() {
    let home = TempDir::new().expect("temporary home");
    let catalogue = presets(home.path());
    let catalogue = catalogue.as_object().expect("catalogue object");

    for (name, preset) in catalogue {
        let object = preset.as_object().expect("preset object");
        if name == "sweep" {
            assert!(
                object.contains_key("deprecated"),
                "the sweep preset must carry `deprecated`"
            );
            let reason = preset["deprecated"].as_str().expect("deprecated reason");
            assert!(reason.contains("loops.sweep"), "{reason}");
            assert!(
                reason.contains("ostrom sweep"),
                "must point at invoking `ostrom sweep`: {reason}"
            );
            assert!(
                reason.to_lowercase().contains("freshness"),
                "must point at pass-time freshness: {reason}"
            );
            // Principle 3: no downstream repository, hosted substrate, customer
            // or URL. These are exactly the shapes such a name would take.
            assert!(!reason.contains("http"), "{reason}");
            assert!(!reason.contains("hub"), "{reason}");
            assert!(!reason.contains("onsager"), "{reason}");
        } else {
            assert!(
                !object.contains_key("deprecated"),
                "{name}: `deprecated` must be absent, not merely null"
            );
        }
    }
}

#[test]
fn each_preset_composes_after_filling_all_placeholders() {
    let source = TempDir::new().unwrap();
    for preset in presets(source.path()).as_object().unwrap().values() {
        let home = TempDir::new().unwrap();
        // Use the shipped files, never invented prompt content.
        command(home.path(), &["init"]);
        let fragment = fill_placeholders(preset);
        fs::write(home.path().join("ostrom.yaml"), fragment.to_yaml().unwrap()).unwrap();
        let composed = compose(home.path());
        for name in fragment.loops.keys() {
            let resolved = composed.resolve_loop(name).expect("adopted loop resolves");
            assert_eq!(
                resolved
                    .repositories
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                ["placeholder-org/adopted-repository"]
            );
        }
    }
}

#[test]
fn init_bytes_and_declarations_cannot_drift_from_presets() {
    let home = TempDir::new().unwrap();
    command(home.path(), &["init"]);
    let bytes = fs::read(home.path().join("ostrom.yaml")).unwrap();
    assert_eq!(bytes, include_bytes!("fixtures/loops/init.yaml"));
    let authored = PolicyManifest::parse_yaml(std::str::from_utf8(&bytes).unwrap()).unwrap();
    assert!(authored.loops.is_empty(), "init keeps loops opt-in");
    let initialized = compose(home.path());
    let catalogue = presets(home.path());
    for name in ["builder", "gatekeeper"] {
        let fragment = fill_placeholders(&catalogue[name]);
        for (key, actor) in &fragment.actors {
            assert_eq!(authored.actors.get(key), Some(actor));
        }
        for (key, operation) in &fragment.operations {
            assert_eq!(authored.operations.get(key), Some(operation));
        }
        for (key, grant) in &fragment.grants {
            assert_eq!(authored.grants.get(key), Some(grant));
        }
        fs::write(home.path().join("ostrom.yaml"), fragment.to_yaml().unwrap()).unwrap();
        // Compose both sides: isolated validation has a known scope divergence.
        let adopted = compose(home.path());
        for (key, actor) in &adopted.actors {
            assert_eq!(initialized.actors.get(key), Some(actor));
        }
        for (key, operation) in &adopted.operations {
            assert_eq!(initialized.operations.get(key), Some(operation));
        }
        for (key, grant) in &adopted.grants {
            assert_eq!(initialized.grants.get(key), Some(grant));
        }
    }
}

#[test]
fn init_writes_exactly_four_files() {
    fn collect_files(root: &Path, directory: &Path, files: &mut Vec<String>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect_files(root, &path, files);
            } else {
                files.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .replace('\\', "/"),
                );
            }
        }
    }

    let home = TempDir::new().unwrap();
    command(home.path(), &["init"]);
    let mut files = Vec::new();
    collect_files(home.path(), home.path(), &mut files);
    files.sort();
    assert_eq!(
        files,
        [
            "ostrom.yaml",
            "prompts/gatekeep.md",
            "prompts/triage.md",
            "prompts/work.md"
        ]
    );
}

#[test]
fn agent_preset_prompt_paths_and_init_bytes_match_shipped_assets() {
    let home = TempDir::new().unwrap();
    command(home.path(), &["init"]);
    let catalogue = presets(home.path());
    for (preset, operation, path, expected) in [
        (
            "builder",
            "build-pass",
            "./prompts/work.md",
            include_str!("../../ostrom-store/assets/prompts/work.md"),
        ),
        (
            "gatekeeper",
            "gate-pass",
            "./prompts/gatekeep.md",
            concat!(
                include_str!("../../ostrom-store/assets/prompts/gatekeep.md"),
                "\n\n",
                include_str!("../../ostrom-store/assets/prompts/merge.md"),
            ),
        ),
        (
            "triage",
            "queue-triage",
            "./prompts/triage.md",
            include_str!("../../ostrom-store/assets/prompts/triage.md"),
        ),
    ] {
        let fragment = fill_placeholders(&catalogue[preset]);
        let steps = &fragment.operations[operation].steps;
        assert_eq!(steps.len(), 1, "{preset}");
        assert_eq!(steps[0].uses, "agent/claude", "{preset}");
        assert_eq!(steps[0].parameters["prompt"]["from"], path, "{preset}");
        let actual = fs::read(home.path().join(path)).expect("init writes preset prompt");
        assert!(
            actual == expected.as_bytes(),
            "{preset}: init prompt differs from shipped asset"
        );
    }
}
