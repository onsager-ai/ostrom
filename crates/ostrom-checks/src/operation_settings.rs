use std::{collections::BTreeSet, fs, path::Path};

use ostrom_core::PolicyManifest;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Every `ostrom <subcommand>` a prompt actually invokes inside a shell fence.
///
/// Prose that merely names a command does not count, and neither does a
/// commented line: the settings a role needs are the commands it runs.
fn invoked_subcommands(markdown: &str) -> BTreeSet<String> {
    let mut commands = BTreeSet::new();
    let mut shell_fence = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if let Some(info) = trimmed.strip_prefix("```") {
            if shell_fence {
                shell_fence = false;
            } else {
                shell_fence = matches!(info.trim(), "sh" | "bash" | "shell");
            }
            continue;
        }
        if !shell_fence || trimmed.starts_with('#') {
            continue;
        }
        if let Some(subcommand) = shell_line_subcommand(line) {
            commands.insert(subcommand.to_owned());
        }
    }
    commands
}

fn shell_line_subcommand(line: &str) -> Option<&str> {
    let mut search_from = 0;
    while let Some(relative) = line.get(search_from..)?.find("ostrom") {
        let index = search_from + relative;
        let before = line.as_bytes().get(index.wrapping_sub(1)).copied();
        let after = line.as_bytes().get(index + "ostrom".len()).copied();
        let word_boundaries = before.is_none_or(|byte| !shell_word_byte(byte))
            && after.is_none_or(|byte| !shell_word_byte(byte));
        if word_boundaries && executable_prefix(&line[..index]) {
            let remainder = line[index + "ostrom".len()..].trim_start();
            let end = remainder
                .find(|character: char| !shell_word_character(character))
                .unwrap_or(remainder.len());
            let subcommand = &remainder[..end];
            if !subcommand.is_empty() {
                return Some(subcommand);
            }
        }
        search_from = index + "ostrom".len();
    }
    None
}

const fn shell_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

const fn shell_word_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
}

fn executable_prefix(prefix: &str) -> bool {
    let prefix = prefix.trim();
    if prefix.is_empty() || prefix.ends_with("$(") {
        return true;
    }
    let segment = prefix
        .rsplit_once("&&")
        .map(|(_, tail)| tail)
        .or_else(|| prefix.rsplit_once("||").map(|(_, tail)| tail))
        .or_else(|| prefix.rsplit_once(';').map(|(_, tail)| tail))
        .unwrap_or(prefix)
        .trim();
    let mut saw_prefix = false;
    for word in segment.split_ascii_whitespace() {
        if matches!(
            word,
            "if" | "then" | "do" | "!" | "command" | "exec" | "env"
        ) || (word.contains('=') && !word.starts_with('='))
        {
            saw_prefix = true;
            continue;
        }
        return false;
    }
    saw_prefix
}

const SETTINGS_SCHEMA: &str = "https://json.schemastore.org/claude-code-settings.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationSettings {
    #[serde(rename = "$schema")]
    schema: String,
    env: OperationEnvironment,
    permissions: OperationPermissions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationEnvironment {
    #[serde(rename = "OSTROM_ACTOR")]
    actor: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationPermissions {
    #[serde(rename = "defaultMode")]
    default_mode: String,
    allow: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OperationSettingsError {
    #[error("unknown actor `{0}`")]
    UnknownActor(String),
    #[error("could not serialize generated settings: {0}")]
    Serialize(String),
    #[error("could not read operation settings at {path}: {message}")]
    Read { path: String, message: String },
    #[error("operation settings at {path} are not valid JSON: {message}")]
    Parse { path: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationSettingsDrift {
    pub expected: Vec<String>,
    pub actual: Vec<String>,
    pub detail: String,
}

/// Render the harness profile derived solely from an actor's operation grants.
///
/// Repository/selectors remain runtime authorization constraints, so any grant
/// for the actor binds the operation once. Denies stay in the manifest and are
/// checked against the resolved target; the harness itself defaults to deny.
pub fn generate_operation_settings(
    manifest: &PolicyManifest,
    actor: &str,
) -> Result<String, OperationSettingsError> {
    let settings = derived_settings(manifest, actor)?;
    let mut source = serde_json::to_string_pretty(&settings)
        .map_err(|error| OperationSettingsError::Serialize(error.to_string()))?;
    source.push('\n');
    Ok(source)
}

pub fn check_operation_settings_drift(
    manifest: &PolicyManifest,
    actor: &str,
    path: &Path,
) -> Result<Option<OperationSettingsDrift>, OperationSettingsError> {
    let expected = derived_settings(manifest, actor)?;
    let source = fs::read_to_string(path).map_err(|error| OperationSettingsError::Read {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    let actual = serde_json::from_str::<OperationSettings>(&source).map_err(|error| {
        OperationSettingsError::Parse {
            path: path.display().to_string(),
            message: error.to_string(),
        }
    })?;
    if expected == actual {
        Ok(None)
    } else {
        Ok(Some(OperationSettingsDrift {
            expected: expected.permissions.allow,
            actual: actual.permissions.allow,
            detail: format!(
                "{} differs from settings derived for actor `{actor}`",
                path.display()
            ),
        }))
    }
}

fn derived_settings(
    manifest: &PolicyManifest,
    actor: &str,
) -> Result<OperationSettings, OperationSettingsError> {
    if !manifest.actors.contains_key(actor) {
        return Err(OperationSettingsError::UnknownActor(actor.to_owned()));
    }
    let operations = manifest
        .operations
        .keys()
        .filter(|operation| actor_has_grant(manifest, actor, operation))
        .map(|operation| format!("Bash(ostrom {operation} *)"))
        .collect::<Vec<_>>();
    Ok(OperationSettings {
        schema: SETTINGS_SCHEMA.to_owned(),
        env: OperationEnvironment {
            actor: actor.to_owned(),
        },
        permissions: OperationPermissions {
            default_mode: "deny".to_owned(),
            allow: operations,
        },
    })
}

fn actor_has_grant(manifest: &PolicyManifest, actor: &str, operation: &str) -> bool {
    manifest.grants.values().any(|grant| {
        (grant.actors.is_empty() || grant.actors.iter().any(|candidate| candidate == actor))
            && (grant.operations.is_empty()
                || grant
                    .operations
                    .iter()
                    .any(|candidate| candidate == operation))
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleSkillOperations {
    pub actor: String,
    pub skills: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillOperationViolation {
    pub actor: String,
    pub skill: String,
    pub operation: String,
    pub detail: String,
}

/// Validate only protocols explicitly enrolled in operation dispatch.
///
/// Existing skills remain on the legacy check until their human-approved
/// migration; callers pass a binding here at the same step that ports a skill.
#[must_use]
pub fn check_skill_operation_grants(
    plugin_root: &Path,
    manifest: &PolicyManifest,
    protocols: &[RoleSkillOperations],
) -> Vec<SkillOperationViolation> {
    let mut violations = Vec::new();
    for protocol in protocols {
        for skill in &protocol.skills {
            let path = plugin_root.join("skills").join(skill).join("SKILL.md");
            let Ok(source) = fs::read_to_string(&path) else {
                violations.push(SkillOperationViolation {
                    actor: protocol.actor.clone(),
                    skill: skill.clone(),
                    operation: String::new(),
                    detail: format!("could not read migrated skill {}", path.display()),
                });
                continue;
            };
            for operation in invoked_subcommands(&source) {
                if !manifest.operations.contains_key(&operation)
                    || !actor_has_grant(manifest, &protocol.actor, &operation)
                {
                    violations.push(SkillOperationViolation {
                        actor: protocol.actor.clone(),
                        skill: skill.clone(),
                        operation: operation.clone(),
                        detail: format!(
                            "skill {skill} invokes `ostrom {operation}` without a grant for actor `{}`",
                            protocol.actor
                        ),
                    });
                }
            }
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ostrom_core::PolicyManifest;
    use tempfile::tempdir;

    use super::{
        RoleSkillOperations, check_operation_settings_drift, check_skill_operation_grants,
        generate_operation_settings, invoked_subcommands,
    };

    const POLICY: &str = "manifest_version: 1\nactors: {builder: {}, gatekeeper: {}}\noperations:\n  comment:\n    steps:\n      - uses: gh/post-verdict\n        with: {note: placeholder}\n  merge:\n    steps:\n      - uses: gh/merge-pr\n        requires: ready\ngrants:\n  builder-comment: {actors: builder, operations: comment}\n  gatekeeper-merge: {actors: gatekeeper, operations: merge}\n";

    #[test]
    fn generated_settings_contain_exactly_granted_operations() {
        let manifest = PolicyManifest::from_yaml(POLICY).expect("manifest");
        let builder = generate_operation_settings(&manifest, "builder").expect("settings");
        assert!(builder.contains("\"defaultMode\": \"deny\""));
        assert!(builder.contains("Bash(ostrom comment *)"));
        assert!(!builder.contains("Bash(ostrom merge *)"));
        assert!(!builder.contains("\"deny\":"));

        let gatekeeper = generate_operation_settings(&manifest, "gatekeeper").expect("settings");
        assert!(gatekeeper.contains("Bash(ostrom merge *)"));
        assert!(!gatekeeper.contains("Bash(ostrom comment *)"));
        assert!(!gatekeeper.contains("GH_TOKEN"));
        assert!(!gatekeeper.contains("GITHUB_TOKEN"));
    }

    #[test]
    fn a_hand_edit_is_reported_as_drift() {
        let manifest = PolicyManifest::from_yaml(POLICY).expect("manifest");
        let root = tempdir().expect("fixture");
        let path = root.path().join("builder.settings.json");
        fs::write(
            &path,
            generate_operation_settings(&manifest, "builder")
                .expect("settings")
                .replace("Bash(ostrom comment *)", "Bash(ostrom hand-edited *)"),
        )
        .expect("write fixture");
        let drift = check_operation_settings_drift(&manifest, "builder", &path)
            .expect("check")
            .expect("drift");
        assert_eq!(drift.actual, ["Bash(ostrom hand-edited *)"]);

        fs::write(
            &path,
            generate_operation_settings(&manifest, "builder")
                .expect("settings")
                .replace(
                    "\"OSTROM_ACTOR\": \"builder\"",
                    "\"OSTROM_ACTOR\": \"builder\", \"extra\": \"placeholder\"",
                ),
        )
        .expect("write extra field");
        assert!(
            check_operation_settings_drift(&manifest, "builder", &path).is_err(),
            "unknown fields are drift, not ignored"
        );
    }

    #[test]
    fn migrated_skill_verb_without_an_authorising_grant_fails() {
        let manifest = PolicyManifest::from_yaml(POLICY).expect("manifest");
        let root = tempdir().expect("fixture");
        let skill = root.path().join("skills/work");
        fs::create_dir_all(&skill).expect("skill directory");
        fs::write(
            skill.join("SKILL.md"),
            "```sh\nostrom merge placeholder\n```\n",
        )
        .expect("skill fixture");
        let violations = check_skill_operation_grants(
            root.path(),
            &manifest,
            &[RoleSkillOperations {
                actor: "builder".to_owned(),
                skills: vec!["work".to_owned()],
            }],
        );
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].operation, "merge");
        assert!(violations[0].detail.contains("without a grant"));
    }
    #[test]
    fn extracts_commands_from_shell_fences_not_prose_or_credential_children() {
        let source = r#"
Never run `ostrom forbidden`.

```sh
ostrom sweep
selected="$(ostrom select-work select owner)"
ostrom credential gatekeeper repo -- ostrom gate repo#1
echo ostrom ignored
```

```json
{"command":"ostrom ignored-too"}
```
"#;
        assert_eq!(
            invoked_subcommands(source).into_iter().collect::<Vec<_>>(),
            ["credential", "select-work", "sweep"]
        );
    }
}
