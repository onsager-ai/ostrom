use std::{fs, path::Path, process::Command};

const RETIRED_ENTRYPOINTS: [&str; 8] = [
    concat!("publish", ".sh"),
    concat!("repair-prs", ".sh"),
    concat!("dispatch", ".sh"),
    concat!("select-work", ".sh"),
    concat!("gate", ".sh"),
    concat!("replay", ".sh"),
    concat!("run-node", ".sh"),
    concat!("pass", ".sh"),
];

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
}

fn approved_historical_reference(path: &Path, line: &str) -> bool {
    match path.to_str() {
        Some("crates/ostrom-cli/src/main.rs") => {
            line == concat!(
                "/// failure `select-work",
                ".sh` had in production: a broken read that looks like a"
            )
        }
        Some("crates/ostrom-store/src/pass.rs") => {
            line == concat!(
                "// pass",
                ".sh used `-x`. A plain is_file() check would let this through to"
            )
        }
        Some("crates/ostrom-cli/tests/fixtures/gate/README.md") => {
            line == concat!(
                "`plugins/ostrom/scripts/gate",
                ".sh` before that script was deleted. The synthetic"
            )
        }
        _ => false,
    }
}

#[test]
fn tracked_operator_text_does_not_name_retired_shell_entrypoints() {
    let root = workspace_root();
    let output = Command::new("git")
        .args([
            "-C",
            root.to_str().expect("UTF-8 workspace path"),
            "ls-files",
            "-z",
        ])
        .output()
        .expect("list tracked files");
    assert!(output.status.success(), "git ls-files failed");

    let mut violations = Vec::new();
    for bytes in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let relative = Path::new(std::str::from_utf8(bytes).expect("UTF-8 tracked path"));
        let Ok(source) = fs::read_to_string(root.join(relative)) else {
            continue;
        };
        for (index, line) in source.lines().enumerate() {
            if RETIRED_ENTRYPOINTS
                .iter()
                .any(|entrypoint| line.contains(entrypoint))
                && !approved_historical_reference(relative, line.trim_start())
            {
                violations.push(format!("{}:{}:{line}", relative.display(), index + 1));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "tracked operator-facing text names retired shell entrypoints:\n{}",
        violations.join("\n")
    );
}
