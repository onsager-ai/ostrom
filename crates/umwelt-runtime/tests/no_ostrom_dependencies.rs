use std::{fs, path::PathBuf};

#[test]
fn runtime_manifest_names_no_ostrom_crate() {
    let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path).expect("read runtime Cargo.toml");
    let offending = manifest
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .find(|line| line.contains("ostrom-"));

    assert!(
        offending.is_none(),
        "{} names a forbidden ostrom-* crate: {}",
        manifest_path.display(),
        offending.unwrap_or_default()
    );
}
