use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
};

#[derive(Debug, PartialEq, Eq)]
pub enum Contents {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
}

pub type Tree = BTreeMap<PathBuf, (u32, Contents)>;

pub fn snapshot(root: &Path) -> Tree {
    fn visit(root: &Path, path: &Path, tree: &mut Tree) {
        let metadata = fs::symlink_metadata(path).expect("snapshot metadata");
        let contents = if metadata.is_symlink() {
            Contents::Symlink(fs::read_link(path).expect("snapshot symlink target"))
        } else if metadata.is_dir() {
            for entry in fs::read_dir(path).expect("snapshot directory") {
                visit(root, &entry.expect("snapshot entry").path(), tree);
            }
            Contents::Directory
        } else {
            assert!(
                metadata.is_file(),
                "unexpected file type: {}",
                path.display()
            );
            Contents::File(fs::read(path).expect("snapshot every file's bytes"))
        };
        tree.insert(
            path.strip_prefix(root).expect("relative path").to_owned(),
            (metadata.permissions().mode(), contents),
        );
    }
    let mut tree = Tree::new();
    visit(root, root, &mut tree);
    tree
}

pub fn assert_unchanged(before: &Tree, after: &Tree) {
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "OSTROM_HOME paths changed"
    );
    for (path, contents) in before {
        assert_eq!(
            Some(contents),
            after.get(path),
            "OSTROM_HOME entry changed: {}",
            path.display()
        );
    }
}
