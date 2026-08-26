use std::{fs, path::PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|crates| crates.parent())
        .expect("workspace root")
        .to_owned()
}

#[test]
fn workflows_derive_both_msrv_outputs_without_a_literal_toolchain() {
    for workflow in ["test.yml", "release.yml"] {
        let path = workspace_root().join(".github/workflows").join(workflow);
        let source = fs::read_to_string(&path).expect("read workflow");

        assert!(
            source.contains("uses: ./.github/actions/resolve-msrv"),
            "{workflow} no longer uses the shared MSRV resolver"
        );
        assert!(
            !source.contains("plugins/ostrom/tests/resolve-msrv.sh"),
            "{workflow} still calls the retired resolver"
        );
        assert!(
            !has_numeric_argument(&source, "toolchain install")
                && !has_numeric_argument(&source, "cargo +"),
            "{workflow} hardcodes a Rust toolchain"
        );
    }

    let action =
        fs::read_to_string(workspace_root().join(".github/actions/resolve-msrv/action.yml"))
            .expect("read MSRV action");
    assert!(action.contains("cargo metadata --no-deps --format-version 1"));
    assert!(action.contains(".packages[].rust_version"));
    assert!(action.contains("declared=%s\\ntoolchain=%s\\n"));
    assert!(action.contains("$GITHUB_OUTPUT"));
    assert!(!has_numeric_argument(&action, "toolchain install"));
    assert!(!has_numeric_argument(&action, "cargo +"));
}

#[test]
fn shell_workflow_guard_calls_the_native_unbypassable_check() {
    let path = workspace_root().join(".github/workflows/test.yml");
    let source = fs::read_to_string(path).expect("read test workflow");

    assert!(source.contains("target/debug/ostrom check shell-retirement"));
    // The plugin-surface check went out with the Claude Code plugin; the guard
    // now asserts CI does not resurrect it alongside the other retired jobs.
    assert!(!source.contains("check plugin-surface"));
    assert!(!source.contains("check skill-version-bump"));
    assert!(!source.contains("plugin-integration:"));
    assert!(!source.contains("bash-bugfix"));
    assert!(!source.contains("PULL_REQUEST_LABELS"));
}

fn has_numeric_argument(source: &str, marker: &str) -> bool {
    source.lines().any(|line| {
        line.split_once(marker).is_some_and(|(_, remainder)| {
            remainder
                .trim_start_matches(|character: char| {
                    character.is_ascii_whitespace() || matches!(character, '\'' | '"')
                })
                .starts_with(|character: char| character.is_ascii_digit())
        })
    })
}
