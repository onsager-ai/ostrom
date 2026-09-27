use std::{fs, process::Command};

use tempfile::tempdir;

fn policy(rules: &str) -> String {
    format!(
        "manifest_version: 1\nactors: {{builder: {{}}}}\noperations: {{work: {{steps: []}}}}\n{rules}"
    )
}

#[test]
fn sweep_still_runs_in_a_repository_that_has_no_manifest_yet() {
    // Until #364 cuts over, the retired surfaces are still the running system.
    // A repository with no `ostrom.yaml` must not stop the loop — but it must
    // also not silently pick up the operator's own policy on the way past.
    let outer = tempdir().expect("outer directory");
    let repository = outer.path().join("repository");
    let home = tempdir().expect("temporary OSTROM_HOME");
    fs::create_dir_all(repository.join(".git")).expect("repository boundary");
    fs::write(
        home.path().join("manifest.yml"),
        policy("grants:\n  operator-grant: {actors: builder, operations: work}\n"),
    )
    .expect("write legacy operator manifest");

    let output = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .current_dir(&repository)
        .env("OSTROM_HOME", home.path())
        .args(["sweep"])
        .output()
        .expect("run sweep without a repository manifest");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("ungoverned"),
        "sweep must tolerate an absent manifest before the cutover: {stderr}"
    );
    assert!(
        !stderr.contains("operator-grant"),
        "an absent manifest must not fall through to operator policy: {stderr}"
    );
}
