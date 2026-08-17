use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParityError {
    #[error("parity sweep requires an explicit scratch OSTROM_HOME")]
    ScratchHomeRequired,
    #[error("parity sweep refuses the live legacy Ostrom home at {0}")]
    LiveHome(String),
    #[error("parity sweep comparison target is missing: {0}")]
    MissingSweepScript(String),
    #[error("could not prepare parity scratch space: {0}")]
    Scratch(String),
    #[error("shell sweep failed: {0}")]
    Shell(String),
    #[error("native sweep failed: {0}")]
    Native(String),
    #[error("could not compare parity queue: {0}")]
    Queue(String),
}

#[derive(Debug, Clone)]
pub struct SweepParityOptions {
    pub source_home: PathBuf,
    pub working_directory: PathBuf,
    pub executable: PathBuf,
    pub plugin_root: PathBuf,
    pub started_at: DateTime<Utc>,
    pub fixture: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SweepParityOutcome {
    pub row_count: usize,
    pub differences: BTreeMap<String, Vec<String>>,
}

impl SweepParityOptions {
    pub fn from_environment(
        working_directory: PathBuf,
        executable: PathBuf,
        plugin_root: PathBuf,
        started_at: DateTime<Utc>,
        fixture: Option<PathBuf>,
    ) -> Result<Self, ParityError> {
        let source_home = env::var_os("OSTROM_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or(ParityError::ScratchHomeRequired)?;
        if let Some(home) = env::var_os("HOME") {
            let live = PathBuf::from(home).join(".claude/ostrom");
            if same_path(&source_home, &live) {
                return Err(ParityError::LiveHome(source_home.display().to_string()));
            }
        }
        Ok(Self {
            source_home,
            working_directory,
            executable,
            plugin_root,
            started_at,
            fixture,
        })
    }
}

pub fn run_sweep_parity(options: &SweepParityOptions) -> Result<SweepParityOutcome, ParityError> {
    let sweep_script = options.plugin_root.join("scripts/sweep.sh");
    if !sweep_script.is_file() {
        return Err(ParityError::MissingSweepScript(
            sweep_script.display().to_string(),
        ));
    }
    if !options.source_home.is_dir() {
        return Err(ParityError::Scratch(format!(
            "OSTROM_HOME is not a directory: {}",
            options.source_home.display()
        )));
    }

    let scratch = tempfile::tempdir().map_err(|error| ParityError::Scratch(error.to_string()))?;
    let native_home = scratch.path().join("native-home");
    let shell_config = scratch.path().join("shell-config");
    let shell_home = shell_config.join("ostrom");
    fs::create_dir_all(&native_home).map_err(scratch_error)?;
    fs::create_dir_all(&shell_home).map_err(scratch_error)?;
    copy_contents(&options.source_home, &native_home).map_err(scratch_error)?;
    copy_contents(&options.source_home, &shell_home).map_err(scratch_error)?;

    // The legacy sweep calls its sibling publish.sh unconditionally. A private
    // script mirror with that one edge replaced makes publication unreachable,
    // even when inherited configuration names a real destination.
    let parity_plugin = scratch.path().join("parity-plugin");
    let parity_scripts = parity_plugin.join("scripts");
    fs::create_dir_all(&parity_scripts).map_err(scratch_error)?;
    copy_contents(&options.plugin_root.join("scripts"), &parity_scripts).map_err(scratch_error)?;
    fs::write(
        parity_scripts.join("publish.sh"),
        "#!/usr/bin/env bash\nexit 3\n",
    )
    .map_err(scratch_error)?;

    let instant = options
        .started_at
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    let shell = Command::new("bash")
        .arg(parity_scripts.join("sweep.sh"))
        .current_dir(&options.working_directory)
        .env_remove("OSTROM_HOME")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("MANDATE_SEMANTIC_DERIVER")
        .env("CLAUDE_CONFIG_DIR", &shell_config)
        .env("CLAUDE_PLUGIN_ROOT", &options.plugin_root)
        .env("MANDATE_SECRETS_FILE", shell_home.join("secrets.yaml"))
        .env("MANDATE_SWEEP_MODE", "auto")
        .env("MANDATE_SWEEP_TIME", &instant)
        .env_remove("MANDATE_SWEEP_ORG")
        .env_remove("MANDATE_SWEEP_WORK")
        .env_remove("MANDATE_SWEEP_MODE_EFFECTIVE")
        .output()
        .map_err(|error| ParityError::Shell(error.to_string()))?;
    if !shell.status.success() {
        return Err(ParityError::Shell(output_detail(&shell.stderr)));
    }

    let mut native = Command::new(&options.executable);
    native.args(["sweep", "--started-at", &instant]);
    if let Some(fixture) = &options.fixture {
        native.arg("--fixture").arg(fixture);
    }
    let native = native
        .current_dir(&options.working_directory)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("MANDATE_SEMANTIC_DERIVER")
        .env("OSTROM_HOME", &native_home)
        .env("OSTROM_PLUGIN_ROOT", &options.plugin_root)
        .env("MANDATE_SECRETS_FILE", native_home.join("secrets.yaml"))
        .output()
        .map_err(|error| ParityError::Native(error.to_string()))?;
    if !native.status.success() {
        return Err(ParityError::Native(output_detail(&native.stderr)));
    }

    compare_queues(
        &shell_home.join("queue.jsonl"),
        &native_home.join("queue.jsonl"),
    )
}

fn compare_queues(shell: &Path, native: &Path) -> Result<SweepParityOutcome, ParityError> {
    let shell = read_rows(shell, "shell")?;
    let native = read_rows(native, "native")?;
    let ids = shell
        .keys()
        .chain(native.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut differences = BTreeMap::<String, Vec<String>>::new();
    for id in &ids {
        let mut shell_fields = BTreeMap::new();
        let mut native_fields = BTreeMap::new();
        if let Some(row) = shell.get(id) {
            flatten_fields("", row, &mut shell_fields);
        }
        if let Some(row) = native.get(id) {
            flatten_fields("", row, &mut native_fields);
        }
        let fields = shell_fields
            .keys()
            .chain(native_fields.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for field in fields {
            if shell_fields.get(&field) != native_fields.get(&field) {
                differences.entry(field).or_default().push(id.clone());
            }
        }
    }
    Ok(SweepParityOutcome {
        row_count: ids.len(),
        differences,
    })
}

fn read_rows(path: &Path, implementation: &str) -> Result<BTreeMap<String, Value>, ParityError> {
    let text = fs::read_to_string(path).map_err(|_| {
        ParityError::Queue(format!(
            "{implementation} sweep did not produce {}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("queue.jsonl")
        ))
    })?;
    let mut rows = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Value = serde_json::from_str(line).map_err(|_| {
            ParityError::Queue(format!(
                "{implementation} queue row {} is malformed",
                index + 1
            ))
        })?;
        let id = row
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                ParityError::Queue(format!(
                    "{implementation} queue row {} has no exact id",
                    index + 1
                ))
            })?
            .to_owned();
        if rows.insert(id.clone(), row).is_some() {
            return Err(ParityError::Queue(format!(
                "{implementation} queue contains duplicate id {id}"
            )));
        }
    }
    Ok(rows)
}

fn flatten_fields(prefix: &str, value: &Value, fields: &mut BTreeMap<String, Value>) {
    if let Value::Object(object) = value {
        if !object.is_empty() {
            for (name, child) in object {
                let path = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}.{name}")
                };
                flatten_fields(&path, child, fields);
            }
            return;
        }
    }
    fields.insert(prefix.to_owned(), value.clone());
}

fn copy_contents(source: &Path, destination: &Path) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&target)?;
            copy_contents(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn scratch_error(error: io::Error) -> ParityError {
    ParityError::Scratch(error.to_string())
}

fn output_detail(stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr).trim().to_owned();
    if detail.is_empty() {
        "process exited without an error message".to_owned()
    } else {
        detail
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    lexical_absolute(left) == lexical_absolute(right)
}

fn lexical_absolute(path: &Path) -> PathBuf {
    use std::path::Component;

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::compare_queues;

    #[test]
    fn comparison_is_keyed_by_exact_id_not_file_position() {
        let root = tempdir().expect("temporary parity queues");
        let shell = root.path().join("shell.jsonl");
        let native = root.path().join("native.jsonl");
        let alpha = r#"{"id":"placeholder-org/alpha#1","age_days":1,"mandate":{"reason":"placeholder alpha"}}"#;
        let beta = r#"{"id":"placeholder-org/alpha#2","age_days":2,"mandate":{"reason":"placeholder beta"}}"#;
        fs::write(&shell, format!("{alpha}\n{beta}\n")).expect("write shell queue");
        fs::write(&native, format!("{beta}\n{alpha}\n")).expect("write reordered queue");

        let outcome = compare_queues(&shell, &native).expect("compare keyed queues");
        assert_eq!(outcome.row_count, 2);
        assert!(outcome.differences.is_empty());
    }

    #[test]
    fn comparison_reports_each_leaf_field_and_its_row_ids() {
        let root = tempdir().expect("temporary parity queues");
        let shell = root.path().join("shell.jsonl");
        let native = root.path().join("native.jsonl");
        fs::write(
            &shell,
            r#"{"id":"placeholder-org/alpha#1","age_days":3,"mandate":{"reason":"shell placeholder"}}
{"id":"placeholder-org/alpha#2","age_days":4,"mandate":{"reason":"same"}}
"#,
        )
        .expect("write shell queue");
        fs::write(
            &native,
            r#"{"id":"placeholder-org/alpha#2","age_days":5,"mandate":{"reason":"same"}}
{"id":"placeholder-org/alpha#1","age_days":3,"mandate":{"reason":"native placeholder"}}
"#,
        )
        .expect("write native queue");

        let outcome = compare_queues(&shell, &native).expect("compare divergent queues");
        assert_eq!(
            outcome.differences["mandate.reason"],
            ["placeholder-org/alpha#1"]
        );
        assert_eq!(outcome.differences["age_days"], ["placeholder-org/alpha#2"]);
    }
}
