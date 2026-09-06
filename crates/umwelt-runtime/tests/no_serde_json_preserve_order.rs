use std::{fs, path::PathBuf};

#[test]
fn workspace_manifest_does_not_enable_serde_json_preserve_order() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_dir = crate_dir
        .parent()
        .and_then(|crates_dir| crates_dir.parent())
        .expect("runtime crate is nested under the workspace root");
    let manifest_path = workspace_dir.join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path).expect("read workspace Cargo.toml");
    let offending = manifest
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .find(|line| line.contains("preserve_order"));

    assert!(
        offending.is_none(),
        "{} enables forbidden serde_json/preserve_order: {}",
        manifest_path.display(),
        offending.unwrap_or_default()
    );
}
