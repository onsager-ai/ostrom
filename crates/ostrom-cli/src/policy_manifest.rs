use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Component, Path, PathBuf},
};

use ostrom_core::{
    ActorDecl, CheckContractError, CheckDocument, LoopDecl, OperationDecl, PolicyManifest,
    RuleDecl, SelectorFinding, SelectorUniverse,
};
use serde::Deserialize;
use serde_yaml::{Mapping, Value};
use thiserror::Error;

pub(crate) fn run_validate(path: &Path, normalized: bool) -> Result<(), PolicyLoadError> {
    let loaded = load(path)?;
    // Resolve the ladder even when normalized output was not requested. This
    // catches a present environment value with the wrong declared type while
    // never placing its raw value in diagnostics.
    loaded
        .resolve_inputs(ostrom_store::environment::declared_input)
        .map_err(|error| PolicyLoadError::Validation(error.to_string()))?;

    let verbs = command_verbs()
        .map(str::to_owned)
        .chain(loaded.operations.keys().cloned());
    let universe = SelectorUniverse::from_manifest(&loaded, verbs);
    let findings = loaded.selector_findings(&universe);
    for finding in &findings {
        if let SelectorFinding::Empty {
            rule,
            selector,
            repository,
            message,
            ..
        } = finding
        {
            eprintln!(
                "finding: rule `{rule}` selector `{selector}`{}: {message}",
                repository
                    .as_deref()
                    .map_or_else(String::new, |value| format!(" in `{value}`"))
            );
        }
    }
    if let Some(finding) = findings.iter().find(|finding| finding.is_error()) {
        return Err(PolicyLoadError::Selector(format_finding(finding)));
    }

    if normalized {
        print!(
            "{}",
            loaded
                .to_yaml()
                .map_err(|error| PolicyLoadError::Validation(error.to_string()))?
        );
    } else {
        println!("valid: {}", path.display());
    }
    Ok(())
}

fn format_finding(finding: &SelectorFinding) -> String {
    match finding {
        SelectorFinding::Error {
            rule,
            selector,
            repository,
            message,
            ..
        }
        | SelectorFinding::Empty {
            rule,
            selector,
            repository,
            message,
            ..
        } => format!(
            "rule `{rule}` selector `{selector}`{}: {message}",
            repository
                .as_deref()
                .map_or_else(String::new, |value| format!(" in `{value}`"))
        ),
    }
}

fn command_verbs() -> impl Iterator<Item = &'static str> {
    [
        "audit",
        "check",
        "config",
        "credential",
        "dispatch",
        "doctor",
        "excuse",
        "gate",
        "hook",
        "implement",
        "lease",
        "local-drift",
        "migrate",
        "parity",
        "pass",
        "plan",
        "queue",
        "repair-prs",
        "replay",
        "select-work",
        "sweep",
        "trace",
        "validate",
        "work-order",
    ]
    .into_iter()
}

pub(crate) fn load(path: &Path) -> Result<PolicyManifest, PolicyLoadError> {
    let source = read(path)?;
    let mut manifest =
        PolicyManifest::parse_yaml(&source).map_err(|source| PolicyLoadError::Yaml {
            path: path.to_path_buf(),
            source,
        })?;
    let includes = std::mem::take(&mut manifest.includes);
    let mut origins = Origins::from_root(&manifest, path);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));

    for pattern in includes {
        let matches = expand_include(parent, &pattern)?;
        if matches.is_empty() {
            return Err(PolicyLoadError::NoIncludeMatch {
                manifest: path.to_path_buf(),
                pattern,
            });
        }
        for include in matches {
            merge_include(&mut manifest, &mut origins, &include)?;
        }
    }
    manifest
        .validate()
        .map_err(|error| PolicyLoadError::Validation(error.to_string()))?;
    validate_check_requirements(&manifest, parent)?;
    Ok(manifest)
}

fn validate_check_requirements(
    manifest: &PolicyManifest,
    manifest_directory: &Path,
) -> Result<(), PolicyLoadError> {
    let requirements = manifest
        .operations
        .iter()
        .flat_map(|(operation, declaration)| {
            declaration.steps.iter().filter_map(move |step| {
                step.requires
                    .as_deref()
                    .map(|check| (operation.as_str(), check))
            })
        })
        .collect::<Vec<_>>();
    if requirements.is_empty() {
        return Ok(());
    }

    let path = manifest_directory.join("checks.yaml");
    let source = read(&path)?;
    let document =
        CheckDocument::from_yaml(&source).map_err(|source| PolicyLoadError::CheckCatalogue {
            path: path.clone(),
            source,
        })?;
    for (operation, check) in requirements {
        if !document.checks.contains_key(check) {
            return Err(PolicyLoadError::UnknownCheck {
                operation: operation.to_owned(),
                check: check.to_owned(),
                path,
            });
        }
    }
    Ok(())
}

fn read(path: &Path) -> Result<String, PolicyLoadError> {
    fs::read_to_string(path).map_err(|source| PolicyLoadError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn merge_include(
    manifest: &mut PolicyManifest,
    origins: &mut Origins,
    path: &Path,
) -> Result<(), PolicyLoadError> {
    let source = read(path)?;
    let value: Value = serde_yaml::from_str(&source).map_err(|source| PolicyLoadError::Yaml {
        path: path.to_path_buf(),
        source,
    })?;
    let mapping = value
        .as_mapping()
        .ok_or_else(|| PolicyLoadError::LeafShape {
            path: path.to_path_buf(),
            message: "included document must be a map".to_owned(),
        })?;
    let markers = ["actor", "operation", "grant", "deny", "loop"]
        .into_iter()
        .filter(|marker| mapping.contains_key(Value::String((*marker).to_owned())))
        .collect::<Vec<_>>();
    if markers.len() > 1 {
        return Err(PolicyLoadError::LeafShape {
            path: path.to_path_buf(),
            message: format!("included leaf has multiple identity keys: {markers:?}"),
        });
    }
    if let Some(marker) = markers.first() {
        merge_leaf(manifest, origins, path, mapping.clone(), marker)
    } else {
        let fragment: IncludeFragment =
            serde_yaml::from_value(value).map_err(|source| PolicyLoadError::Yaml {
                path: path.to_path_buf(),
                source,
            })?;
        merge_map(
            &mut manifest.actors,
            fragment.actors,
            "actor",
            path,
            &mut origins.actors,
        )?;
        merge_map(
            &mut manifest.operations,
            fragment.operations,
            "operation",
            path,
            &mut origins.operations,
        )?;
        merge_map(
            &mut manifest.grants,
            fragment.grants,
            "grant",
            path,
            &mut origins.grants,
        )?;
        merge_map(
            &mut manifest.denies,
            fragment.denies,
            "deny",
            path,
            &mut origins.denies,
        )?;
        merge_map(
            &mut manifest.loops,
            fragment.loops,
            "loop",
            path,
            &mut origins.loops,
        )
    }
}

fn merge_leaf(
    manifest: &mut PolicyManifest,
    origins: &mut Origins,
    path: &Path,
    mut mapping: Mapping,
    marker: &str,
) -> Result<(), PolicyLoadError> {
    let key = Value::String(marker.to_owned());
    let id = mapping
        .remove(&key)
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| PolicyLoadError::LeafShape {
            path: path.to_path_buf(),
            message: format!("`{marker}` identity must be a string"),
        })?;
    let body = Value::Mapping(mapping);
    match marker {
        "actor" => insert_leaf(
            &mut manifest.actors,
            &mut origins.actors,
            id,
            parse_leaf(body, path)?,
            "actor",
            path,
        ),
        "operation" => insert_leaf(
            &mut manifest.operations,
            &mut origins.operations,
            id,
            parse_leaf(body, path)?,
            "operation",
            path,
        ),
        "grant" => insert_leaf(
            &mut manifest.grants,
            &mut origins.grants,
            id,
            parse_leaf(body, path)?,
            "grant",
            path,
        ),
        "deny" => insert_leaf(
            &mut manifest.denies,
            &mut origins.denies,
            id,
            parse_leaf(body, path)?,
            "deny",
            path,
        ),
        "loop" => insert_leaf(
            &mut manifest.loops,
            &mut origins.loops,
            id,
            parse_leaf(body, path)?,
            "loop",
            path,
        ),
        _ => unreachable!("marker came from closed list"),
    }
}

fn parse_leaf<T: for<'de> Deserialize<'de>>(
    body: Value,
    path: &Path,
) -> Result<T, PolicyLoadError> {
    serde_yaml::from_value(body).map_err(|source| PolicyLoadError::Yaml {
        path: path.to_path_buf(),
        source,
    })
}

fn insert_leaf<T>(
    target: &mut BTreeMap<String, T>,
    origins: &mut BTreeMap<String, PathBuf>,
    id: String,
    value: T,
    kind: &'static str,
    path: &Path,
) -> Result<(), PolicyLoadError> {
    if let Some(first) = origins.get(&id) {
        return Err(PolicyLoadError::Collision {
            kind,
            id,
            first: first.clone(),
            second: path.to_path_buf(),
        });
    }
    target.insert(id.clone(), value);
    origins.insert(id, path.to_path_buf());
    Ok(())
}

fn merge_map<T>(
    target: &mut BTreeMap<String, T>,
    incoming: BTreeMap<String, T>,
    kind: &'static str,
    path: &Path,
    origins: &mut BTreeMap<String, PathBuf>,
) -> Result<(), PolicyLoadError> {
    for (id, value) in incoming {
        insert_leaf(target, origins, id, value, kind, path)?;
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct IncludeFragment {
    #[serde(default)]
    actors: BTreeMap<String, ActorDecl>,
    #[serde(default)]
    operations: BTreeMap<String, OperationDecl>,
    #[serde(default)]
    grants: BTreeMap<String, RuleDecl>,
    #[serde(default)]
    denies: BTreeMap<String, RuleDecl>,
    #[serde(default)]
    loops: BTreeMap<String, LoopDecl>,
}

#[derive(Debug, Default)]
struct Origins {
    actors: BTreeMap<String, PathBuf>,
    operations: BTreeMap<String, PathBuf>,
    grants: BTreeMap<String, PathBuf>,
    denies: BTreeMap<String, PathBuf>,
    loops: BTreeMap<String, PathBuf>,
}

impl Origins {
    fn from_root(manifest: &PolicyManifest, path: &Path) -> Self {
        let origins = |keys: Vec<String>| {
            keys.into_iter()
                .map(|key| (key, path.to_path_buf()))
                .collect()
        };
        Self {
            actors: origins(manifest.actors.keys().cloned().collect()),
            operations: origins(manifest.operations.keys().cloned().collect()),
            grants: origins(manifest.grants.keys().cloned().collect()),
            denies: origins(manifest.denies.keys().cloned().collect()),
            loops: origins(manifest.loops.keys().cloned().collect()),
        }
    }
}

fn expand_include(parent: &Path, pattern: &str) -> Result<Vec<PathBuf>, PolicyLoadError> {
    let pattern_path = Path::new(pattern);
    if pattern_path.is_absolute()
        || pattern_path
            .components()
            .any(|component| component == Component::ParentDir)
    {
        return Err(PolicyLoadError::UnsafeInclude(pattern.to_owned()));
    }
    if !pattern.contains(['*', '?']) {
        return Ok(vec![parent.join(pattern)]);
    }
    let wildcard = pattern.find(['*', '?']).expect("glob contains a wildcard");
    let prefix = Path::new(&pattern[..wildcard]);
    let base = parent.join(prefix.parent().unwrap_or_else(|| Path::new(".")));
    if !base.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    collect_files(&base, &mut files)?;
    files.retain(|path| {
        path.strip_prefix(parent)
            .ok()
            .and_then(Path::to_str)
            .is_some_and(|relative| include_glob(relative, pattern))
    });
    files.sort();
    Ok(files)
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), PolicyLoadError> {
    for entry in fs::read_dir(directory).map_err(|source| PolicyLoadError::Io {
        path: directory.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| PolicyLoadError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| PolicyLoadError::Io {
            path: path.clone(),
            source,
        })?;
        if file_type.is_dir() && !file_type.is_symlink() {
            if path.file_name().is_none_or(|name| name != ".git") {
                collect_files(&path, files)?;
            }
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn include_glob(value: &str, pattern: &str) -> bool {
    let value = value.as_bytes();
    let pattern = pattern.as_bytes();
    let mut states = BTreeSet::from([(0_usize, 0_usize)]);
    let mut seen = BTreeSet::new();
    while let Some((value_index, pattern_index)) = states.pop_first() {
        if !seen.insert((value_index, pattern_index)) {
            continue;
        }
        if pattern_index == pattern.len() {
            if value_index == value.len() {
                return true;
            }
            continue;
        }
        if pattern[pattern_index] == b'*' {
            let double = pattern.get(pattern_index + 1) == Some(&b'*');
            let next_pattern = pattern_index + usize::from(double) + 1;
            let skip_pattern = if double && pattern.get(next_pattern) == Some(&b'/') {
                next_pattern + 1
            } else {
                next_pattern
            };
            states.insert((value_index, skip_pattern));
            if value_index < value.len() && (double || value[value_index] != b'/') {
                states.insert((value_index + 1, pattern_index));
            }
        } else if value_index < value.len()
            && (pattern[pattern_index] == value[value_index]
                || (pattern[pattern_index] == b'?' && value[value_index] != b'/'))
        {
            states.insert((value_index + 1, pattern_index + 1));
        }
    }
    false
}

#[derive(Debug, Error)]
pub(crate) enum PolicyLoadError {
    #[error("could not read `{}`: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not parse `{}`: {source}", path.display())]
    Yaml {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("could not load check catalogue `{}`: {source}", path.display())]
    CheckCatalogue {
        path: PathBuf,
        #[source]
        source: CheckContractError,
    },
    #[error(
        "operation `{operation}` requires undefined check `{check}` from `{}`",
        path.display()
    )]
    UnknownCheck {
        operation: String,
        check: String,
        path: PathBuf,
    },
    #[error("invalid policy manifest: {0}")]
    Validation(String),
    #[error("invalid selector: {0}")]
    Selector(String),
    #[error("unsafe include path `{0}`")]
    UnsafeInclude(String),
    #[error("include `{pattern}` from `{}` matched no files", manifest.display())]
    NoIncludeMatch { manifest: PathBuf, pattern: String },
    #[error("invalid included leaf `{}`: {message}", path.display())]
    LeafShape { path: PathBuf, message: String },
    #[error(
        "duplicate {kind} id `{id}` in `{}` and `{}`",
        first.display(),
        second.display()
    )]
    Collision {
        kind: &'static str,
        id: String,
        first: PathBuf,
        second: PathBuf,
    },
}
