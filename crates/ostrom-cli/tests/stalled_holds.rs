use std::{fs, path::PathBuf, process::Command};

use tempfile::TempDir;

mod support;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/policy-explain")
        .join(name)
}

#[test]
fn replaying_344_and_351_produces_two_stalled_hold_findings_in_the_digest() {
    let home = TempDir::new().expect("temporary OSTROM_HOME");
    fs::create_dir(home.path().join(".git")).expect("repository boundary");
    for name in ["checks.yaml", "mandates.yaml"] {
        fs::copy(fixture(name), home.path().join(name)).expect("copy policy fixture");
    }
    fs::copy(fixture("manifest.yml"), home.path().join("ostrom.yaml"))
        .expect("copy policy manifest");
    let trusted_keys = support::sign_manifest(&home.path().join("ostrom.yaml"));
    for started_at in ["2026-08-01T00:00:00Z", "2026-08-09T00:00:00Z"] {
        let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
            .current_dir(home.path())
            .env("OSTROM_HOME", home.path())
            .env("OSTROM_POLICY_TRUSTED_KEYS", &trusted_keys)
            .args([
                "sweep",
                "--fixture",
                fixture("github.json").to_str().expect("UTF-8 fixture path"),
                "--started-at",
                started_at,
            ])
            .output()
            .expect("run policy-hold sweep");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(home.path().join("state.json")).expect("read sweep state"),
    )
    .expect("parse sweep state");
    let findings = state["stalled_holds"]
        .as_array()
        .expect("stalled-hold findings");
    assert_eq!(findings.len(), 2, "{state:#}");
    assert_eq!(findings[0]["id"], "onsager-ai/ostrom#344");
    assert_eq!(findings[1]["id"], "onsager-ai/ostrom#351");
    assert!(findings.iter().all(|finding| finding["verdict"] == "HOLD"));

    let digest = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .env("OSTROM_HOME", home.path())
        .env("OSTROM_POLICY_TRUSTED_KEYS", &trusted_keys)
        .env("MANDATE_NOW_EPOCH", "0")
        .env("MANDATE_DIGEST_TIME", "2026-08-09T00:00:00Z")
        .env("MANDATE_TODAY", "2026-08-09")
        .args(["hook", "digest"])
        .output()
        .expect("render digest");
    assert!(digest.status.success());
    let stdout = String::from_utf8(digest.stdout).expect("UTF-8 digest");
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect("digest envelope");
    let message = envelope["systemMessage"]
        .as_str()
        .expect("digest systemMessage");
    assert!(
        message.contains("STALLED HOLDS — DECIDE OR CHANGE THE RULE"),
        "{stdout}"
    );
    assert_eq!(
        message
            .matches("held 8 days; decide, or change rule")
            .count(),
        2
    );
    assert!(message.contains("onsager-ai/ostrom#344"), "{stdout}");
    assert!(message.contains("onsager-ai/ostrom#351"), "{stdout}");
}
