use std::{fs, path::PathBuf};

#[test]
fn capture_manifest_names_no_ostrom_crate() {
    let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path).expect("read capture Cargo.toml");
    // This textual guard cannot see a transitive path to ostrom-core through
    // another dependency. Closing that gap with cargo metadata is out of scope
    // for this scaffold.
    let offending = manifest
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .find(|line| line.contains("ostrom-") || line.contains("ostrom_"));

    assert!(
        offending.is_none(),
        "{} names a forbidden ostrom[-_]* crate: {}",
        manifest_path.display(),
        offending.unwrap_or_default()
    );
}
