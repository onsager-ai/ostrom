use std::{fs, path::Path};

use ostrom_core::PolicyManifest;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::role_allowlists::invoked_subcommands;

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
        generate_operation_settings,
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
}
