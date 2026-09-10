use std::fs;
use std::path::{Path, PathBuf};

use umwelt_capture::golden::read_metadata;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

#[test]
fn every_committed_fixture_metadata_document_parses() {
    let mut metadata_paths = Vec::new();
    collect_metadata(Path::new(FIXTURES), &mut metadata_paths);
    metadata_paths.sort();

    assert_eq!(
        metadata_paths.len(),
        5,
        "the committed fixture inventory changed; review every new metadata document"
    );
    for path in metadata_paths {
        read_metadata(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    }
}

fn collect_metadata(directory: &Path, paths: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("read fixture directory") {
        let path = entry.expect("read fixture entry").path();
        if path.is_dir() {
            collect_metadata(&path, paths);
        } else if path.file_name().is_some_and(|name| name == "meta.toml") {
            paths.push(path);
        }
    }
}
