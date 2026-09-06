//! Conversions between Ostrom-owned types and the harness runtime boundary.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use ostrom_core::ResolvedLoopCeilings;
use umwelt_runtime::{PassState, TraceAppend};

use crate::{
    StoreError, environment, event_store::append_trace_event, io_error, set_private_file_mode,
};

/// Convert Ostrom's resolved policy ceilings into harness runtime ceilings.
#[must_use]
pub const fn run_ceilings(ceilings: ResolvedLoopCeilings) -> umwelt_runtime::RunCeilings {
    umwelt_runtime::RunCeilings {
        concurrent: ceilings.concurrent,
        spend_usd: ceilings.spend_usd,
        tokens: ceilings.tokens,
    }
}

/// Resolve the governor-owned Node fallback setting before constructing a harness.
#[must_use]
pub fn node_fallbacks() -> Vec<PathBuf> {
    environment::OSTROM_NODE_FALLBACKS.value_os().map_or_else(
        || {
            let mut paths = vec![
                PathBuf::from("/usr/local/bin/node"),
                PathBuf::from("/opt/homebrew/bin/node"),
            ];
            if let Some(home) = environment::HOME
                .value_os()
                .filter(|value| !value.is_empty())
            {
                paths.push(PathBuf::from(home).join(".local/bin/node"));
            }
            paths
        },
        |paths| {
            paths
                .to_string_lossy()
                .split_whitespace()
                .map(PathBuf::from)
                .collect()
        },
    )
}

/// Append through Umwelt while retaining Ostrom's fact-ledger side effect.
pub fn append_trace(path: &Path, record: &TraceAppend) -> Result<Vec<u8>, StoreError> {
    let mut bytes = Vec::new();
    umwelt_runtime::append_trace(&mut bytes, record).map_err(trace_error)?;
    append_trace_event(path, &record.ts, &record.kind, &record.fact)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| io_error("create trace directory", parent, error))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| io_error("open trace", path, error))?;
    set_private_file_mode(path)?;
    file.write_all(&bytes)
        .map_err(|error| io_error("append trace", path, error))?;
    Ok(bytes)
}

fn trace_error(error: umwelt_runtime::TraceAppendError) -> StoreError {
    match error {
        umwelt_runtime::TraceAppendError::Malformed => StoreError::MalformedTrace {
            message: "trace ts and kind must not be empty".to_owned(),
        },
        umwelt_runtime::TraceAppendError::TooLarge { bytes } => StoreError::TraceTooLarge { bytes },
        umwelt_runtime::TraceAppendError::Write(source) => StoreError::Io {
            operation: "serialize trace",
            path: "memory".to_owned(),
            source,
        },
    }
}

pub fn read_pass_state(root: &Path, role: &str) -> Result<Option<PassState>, StoreError> {
    umwelt_runtime::read_pass_state(root, role).map_err(pass_state_error)
}

pub fn write_pass_state(root: &Path, role: &str, state: &PassState) -> Result<(), StoreError> {
    umwelt_runtime::write_pass_state(root, role, state).map_err(pass_state_error)
}

fn pass_state_error(error: umwelt_runtime::PassStateError) -> StoreError {
    match error {
        umwelt_runtime::PassStateError::Io {
            operation,
            path,
            source,
        } => StoreError::Io {
            operation,
            path,
            source,
        },
        umwelt_runtime::PassStateError::Malformed { role, message } => {
            StoreError::MalformedPassState { role, message }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::{Path, PathBuf},
        process::Command,
    };

    use serde_json::{Map, json};
    use tempfile::tempdir;
    use umwelt_runtime::{AgentRunner, CodexHarness};

    use ostrom_core::ResolvedLoopCeilings;

    use super::{append_trace, read_pass_state, run_ceilings, write_pass_state};
    use crate::{PassState, TraceAppend};

    #[test]
    fn trace_append_bytes_agree_with_umwelt() {
        let directory = tempdir().expect("temp dir");
        let path = directory.path().join("sprint.jsonl");
        let record = TraceAppend {
            ts: "2030-01-02T03:04:05Z".to_owned(),
            kind: "work-failed".to_owned(),
            fact: Map::from_iter([
                ("order_id".to_owned(), json!("synthetic-order")),
                ("reason".to_owned(), json!("operator-facing explanation")),
            ]),
            narration: Map::from_iter([(
                "detail".to_owned(),
                json!("local narration remains local"),
            )]),
        };

        let ostrom_bytes = append_trace(&path, &record).expect("append through ostrom");
        let mut umwelt_output = Vec::new();
        let umwelt_bytes = umwelt_runtime::append_trace(&mut umwelt_output, &record)
            .expect("append through umwelt");

        assert_eq!(ostrom_bytes, umwelt_bytes);
        assert_eq!(fs::read(path).expect("read ostrom trace"), umwelt_output);
    }

    #[test]
    fn trace_append_preserves_authored_object_order() {
        let directory = tempdir().expect("temp dir");
        let path = directory.path().join("sprint.jsonl");
        let record = TraceAppend {
            ts: "2030-01-02T03:04:05Z".to_owned(),
            kind: "ordered".to_owned(),
            fact: serde_json::from_str(r#"{"z":1,"a":2}"#).expect("ordered fact"),
            narration: Map::new(),
        };

        append_trace(&path, &record).expect("append ordered trace");

        assert_eq!(
            fs::read_to_string(path).expect("read ordered trace"),
            concat!(
                r#"{"ts":"2030-01-02T03:04:05Z","kind":"ordered","fact":{"z":1,"a":2},"narration":{}}"#,
                "\n"
            )
        );
    }

    #[test]
    fn resolved_ceilings_agree_with_umwelt_shape() {
        let ceilings = run_ceilings(ResolvedLoopCeilings {
            concurrent: Some(2),
            spend_usd: Some(50.5),
            tokens: Some(200_000),
        });

        assert_eq!(ceilings.concurrent, Some(2));
        assert_eq!(ceilings.spend_usd, Some(50.5));
        assert_eq!(ceilings.tokens, Some(200_000));
    }

    #[test]
    fn pass_state_bytes_agree_with_umwelt_fixture_contract() {
        let directory = tempdir().expect("temp dir");
        let state = PassState {
            role_id: "89abcdef".to_owned(),
            wake: 42,
            dispatchability_hash: Some(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
            ),
        };

        write_pass_state(directory.path(), "builder", &state).expect("write pass state");

        assert_eq!(
            fs::read(directory.path().join("builder-pass-id")).expect("read pass id"),
            b"89abcdef\n"
        );
        assert_eq!(
            fs::read(directory.path().join("builder-wake-counter")).expect("read wake counter"),
            b"42\n"
        );
        assert_eq!(
            fs::read(directory.path().join("builder-dispatchability-hash"))
                .expect("read dispatchability hash"),
            b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n"
        );
    }

    #[test]
    fn pre_digest_pass_state_remains_readable() {
        let directory = tempdir().expect("temp dir");
        fs::write(directory.path().join("builder-pass-id"), "0123abcd\n").expect("write id");
        fs::write(directory.path().join("builder-wake-counter"), "9\n").expect("write wake");

        assert_eq!(
            read_pass_state(directory.path(), "builder").expect("read state"),
            Some(PassState {
                role_id: "0123abcd".to_owned(),
                wake: 9,
                dispatchability_hash: None,
            })
        );
    }

    #[cfg(unix)]
    fn executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        fs::create_dir_all(path.parent().expect("executable parent")).expect("create parent");
        fs::write(path, "#!/bin/sh\nexit 0\n").expect("write executable");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod executable");
    }

    #[test]
    fn node_resolution_child() {
        let Some(codex) = env::var_os("OSTROM_NODE_TEST_CODEX") else {
            return;
        };
        let fallbacks = env::var_os("OSTROM_NODE_TEST_FALLBACKS")
            .map(|value| env::split_paths(&value).collect())
            .unwrap_or_default();
        let harness = CodexHarness::new(codex, "fixture-v1", "fixture-model", fallbacks);
        let Some(launch) = harness.prepare().ok() else {
            println!("NODE_PARENT=NONE");
            return;
        };
        let path = launch
            .environment()
            .iter()
            .find(|(name, _)| name == "PATH")
            .map(|(_, value)| value)
            .expect("prepared PATH");
        let parent = env::split_paths(path).next().expect("resolved Node parent");
        println!("NODE_PARENT={}", parent.display());
    }

    #[cfg(unix)]
    fn resolved_node(
        root: &Path,
        nvm: &Path,
        fnm: &Path,
        volta: &Path,
        asdf: &Path,
        fallbacks: &[PathBuf],
    ) -> Option<PathBuf> {
        let codex = root.join("codex-fixture");
        executable(&codex);
        let path = root.join("path");
        fs::create_dir_all(&path).expect("PATH directory");
        let output = Command::new(env::current_exe().expect("current test binary"))
            .args([
                "--exact",
                "umwelt_edge::tests::node_resolution_child",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", path)
            .env("HOME", root.join("home"))
            .env("NVM_DIR", nvm)
            .env("FNM_DIR", fnm)
            .env("VOLTA_HOME", volta)
            .env("ASDF_DATA_DIR", asdf)
            .env("OSTROM_NODE_TEST_CODEX", codex)
            .env(
                "OSTROM_NODE_TEST_FALLBACKS",
                env::join_paths(fallbacks).expect("fallback paths"),
            )
            .output()
            .expect("run node resolver child");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("child stdout")
            .lines()
            .find_map(|line| line.strip_prefix("NODE_PARENT="))
            .filter(|path| *path != "NONE")
            .map(|path| PathBuf::from(path).join("node"))
    }

    #[cfg(unix)]
    #[test]
    fn node_resolution_is_first_hit_wins_across_every_supported_layout() {
        let root = tempdir().expect("temporary node resolution fixture");
        let path_node = root.path().join("path/node");
        let nvm = root.path().join("nvm");
        let older_nvm_node = nvm.join("versions/node/v20.8.9/bin/node");
        let nvm_node = nvm.join("versions/node/v20.10.1/bin/node");
        let fnm = root.path().join("fnm");
        let fnm_node = fnm.join("aliases/default/bin/node");
        let legacy_fnm_node = root.path().join("home/.fnm/aliases/default/bin/node");
        let volta = root.path().join("volta");
        let volta_node = volta.join("bin/node");
        let asdf = root.path().join("asdf");
        let asdf_node = asdf.join("shims/node");
        let standalone_node = root.path().join("standalone/node");
        let resolve = || {
            resolved_node(
                root.path(),
                &nvm,
                &fnm,
                &volta,
                &asdf,
                std::slice::from_ref(&standalone_node),
            )
        };

        assert_eq!(resolve(), None);
        executable(&standalone_node);
        assert_eq!(resolve(), Some(standalone_node.clone()));
        executable(&asdf_node);
        assert_eq!(resolve(), Some(asdf_node.clone()));
        executable(&volta_node);
        assert_eq!(resolve(), Some(volta_node.clone()));
        executable(&legacy_fnm_node);
        assert_eq!(resolve(), Some(legacy_fnm_node.clone()));
        executable(&fnm_node);
        assert_eq!(resolve(), Some(fnm_node.clone()));
        fs::create_dir_all(nvm.join("alias")).expect("create nvm alias directory");
        fs::write(nvm.join("alias/default"), "  v20 \nignored\n").expect("write major alias");
        executable(&older_nvm_node);
        executable(&nvm_node);
        assert_eq!(resolve(), Some(nvm_node.clone()));
        executable(&path_node);
        assert_eq!(resolve(), Some(path_node));
    }

    #[cfg(unix)]
    #[test]
    fn nvm_resolution_uses_only_the_default_alias() {
        let root = tempdir().expect("temporary nvm resolution fixture");
        let nvm = root.path().join("nvm");
        let default_node = nvm.join("versions/node/v18.19.1/bin/node");
        let newer_node = nvm.join("versions/node/v22.1.0/bin/node");
        executable(&default_node);
        executable(&newer_node);
        fs::create_dir_all(nvm.join("alias")).expect("create nvm alias directory");
        fs::write(nvm.join("alias/default"), " v18.19.1 \n").expect("write exact alias");

        assert_eq!(
            resolved_node(
                root.path(),
                &nvm,
                &root.path().join("fnm"),
                &root.path().join("volta"),
                &root.path().join("asdf"),
                &[],
            ),
            Some(default_node)
        );

        fs::write(nvm.join("alias/default"), "node\n").expect("write unsupported alias");
        assert_eq!(
            resolved_node(
                root.path(),
                &nvm,
                &root.path().join("fnm"),
                &root.path().join("volta"),
                &root.path().join("asdf"),
                &[],
            ),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_major_version_alias_resolves_to_the_newest_matching_install() {
        let root = tempdir().expect("temporary nvm major alias fixture");
        let nvm = root.path().join("nvm");
        for version in ["v22.22.3", "v24.9.0", "v24.15.0", "v24.18.0"] {
            executable(&nvm.join(format!("versions/node/{version}/bin/node")));
        }
        fs::create_dir_all(nvm.join("alias")).expect("create nvm alias directory");
        fs::write(nvm.join("alias/default"), "24\n").expect("write major alias");

        assert_eq!(
            resolved_node(
                root.path(),
                &nvm,
                &root.path().join("fnm"),
                &root.path().join("volta"),
                &root.path().join("asdf"),
                &[],
            ),
            Some(nvm.join("versions/node/v24.18.0/bin/node"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_major_alias_skips_a_version_whose_binary_is_missing() {
        let root = tempdir().expect("temporary nvm partial install fixture");
        let nvm = root.path().join("nvm");
        let present = nvm.join("versions/node/v24.15.0/bin/node");
        executable(&present);
        fs::create_dir_all(nvm.join("versions/node/v24.18.0/bin"))
            .expect("create version directory with no binary");
        fs::create_dir_all(nvm.join("alias")).expect("create nvm alias directory");
        fs::write(nvm.join("alias/default"), "24\n").expect("write major alias");

        assert_eq!(
            resolved_node(
                root.path(),
                &nvm,
                &root.path().join("fnm"),
                &root.path().join("volta"),
                &root.path().join("asdf"),
                &[],
            ),
            Some(present)
        );
    }
}
