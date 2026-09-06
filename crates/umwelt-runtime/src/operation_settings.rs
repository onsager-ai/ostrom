use std::{collections::BTreeMap, fs, path::Path};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::LoopCeilings;

const SETTINGS_SCHEMA: &str = "https://json.schemastore.org/claude-code-settings.json";

/// The already-resolved profile content a harness consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessProfile {
    pub environment: BTreeMap<String, String>,
    pub default_mode: String,
    pub allow: Vec<String>,
}

/// Operation inputs already resolved by a consumer for one harness run.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedOperationSettings {
    pub prompt: String,
    pub permission_mode: String,
    pub profile: HarnessProfile,
    pub ceilings: LoopCeilings,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct OperationSettingsRef<'a> {
    #[serde(rename = "$schema")]
    schema: &'static str,
    env: &'a BTreeMap<String, String>,
    permissions: OperationPermissionsRef<'a>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct OperationPermissionsRef<'a> {
    #[serde(rename = "defaultMode")]
    default_mode: &'a str,
    allow: &'a [String],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationSettingsDocument {
    #[serde(rename = "$schema")]
    schema: String,
    env: BTreeMap<String, String>,
    permissions: OperationPermissionsDocument,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationPermissionsDocument {
    #[serde(rename = "defaultMode")]
    default_mode: String,
    allow: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OperationSettingsError {
    #[error("could not serialize generated operation settings: {0}")]
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

/// Render the harness profile from inputs a consumer has already resolved.
pub fn generate_operation_settings(
    settings: &ResolvedOperationSettings,
) -> Result<String, OperationSettingsError> {
    let profile = OperationSettingsRef {
        schema: SETTINGS_SCHEMA,
        env: &settings.profile.environment,
        permissions: OperationPermissionsRef {
            default_mode: &settings.profile.default_mode,
            allow: &settings.profile.allow,
        },
    };
    let mut source = serde_json::to_string_pretty(&profile)
        .map_err(|error| OperationSettingsError::Serialize(error.to_string()))?;
    source.push('\n');
    Ok(source)
}

pub fn check_operation_settings_drift(
    expected: &ResolvedOperationSettings,
    path: &Path,
) -> Result<Option<OperationSettingsDrift>, OperationSettingsError> {
    let source = fs::read_to_string(path).map_err(|error| OperationSettingsError::Read {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    let actual = serde_json::from_str::<OperationSettingsDocument>(&source).map_err(|error| {
        OperationSettingsError::Parse {
            path: path.display().to_string(),
            message: error.to_string(),
        }
    })?;
    let expected_profile = &expected.profile;
    let matches = actual.schema == SETTINGS_SCHEMA
        && actual.env == expected_profile.environment
        && actual.permissions.default_mode == expected_profile.default_mode
        && actual.permissions.allow == expected_profile.allow;
    if matches {
        Ok(None)
    } else {
        Ok(Some(OperationSettingsDrift {
            expected: expected_profile.allow.clone(),
            actual: actual.permissions.allow,
            detail: format!(
                "{} differs from resolved operation settings",
                path.display()
            ),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use tempfile::tempdir;

    use super::{
        HarnessProfile, ResolvedOperationSettings, check_operation_settings_drift,
        generate_operation_settings,
    };
    use crate::LoopCeilings;

    fn settings() -> ResolvedOperationSettings {
        ResolvedOperationSettings {
            prompt: "Resolve the selected operation.".to_owned(),
            permission_mode: "auto".to_owned(),
            profile: HarnessProfile {
                environment: BTreeMap::from([("HARNESS_ACTOR".to_owned(), "builder".to_owned())]),
                default_mode: "deny".to_owned(),
                allow: vec!["Bash(ostrom comment *)".to_owned()],
            },
            ceilings: LoopCeilings {
                concurrent: Some(2),
                spend_usd: Some(50.0),
                tokens: Some(200_000),
            },
        }
    }

    #[test]
    fn generated_settings_contain_exactly_the_resolved_profile() {
        let source = generate_operation_settings(&settings()).expect("settings");
        assert!(source.contains("\"defaultMode\": \"deny\""));
        assert!(source.contains("Bash(ostrom comment *)"));
        assert!(!source.contains("Bash(ostrom merge *)"));
        assert!(!source.contains("GH_TOKEN"));
        assert!(!source.contains("GITHUB_TOKEN"));
        assert!(!source.contains("Resolve the selected operation."));
    }

    #[test]
    fn a_hand_edit_is_reported_as_data_drift() {
        let root = tempdir().expect("fixture");
        let path = root.path().join("builder.settings.json");
        fs::write(
            &path,
            generate_operation_settings(&settings())
                .expect("settings")
                .replace("Bash(ostrom comment *)", "Bash(ostrom hand-edited *)"),
        )
        .expect("write fixture");

        let drift = check_operation_settings_drift(&settings(), &path)
            .expect("check")
            .expect("drift");
        assert_eq!(drift.actual, ["Bash(ostrom hand-edited *)"]);

        fs::write(
            &path,
            generate_operation_settings(&settings())
                .expect("settings")
                .replace("\"env\": {", "\"extra\": true,\n  \"env\": {"),
        )
        .expect("write extra field");
        assert!(
            check_operation_settings_drift(&settings(), &path).is_err(),
            "unknown fields are drift, not ignored"
        );
    }
}
