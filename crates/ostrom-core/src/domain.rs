use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RepositoryName(String);

impl RepositoryName {
    /// Construct a validated `owner/name` repository identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        let mut parts = value.split('/');
        if matches!((parts.next(), parts.next(), parts.next()), (Some(owner), Some(name), None)
            if !owner.is_empty()
                && !name.is_empty()
                && !value.chars().any(char::is_whitespace))
        {
            Ok(Self(value))
        } else {
            Err(DomainError::Repository(value))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RepositoryName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for RepositoryName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DomainError {
    #[error("repository must have the shape owner/name: {0}")]
    Repository(String),
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not parse YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("invalid Ostrom config: {0}")]
    Invalid(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Selector(String);

impl Selector {
    pub fn new(value: impl Into<String>) -> Result<Self, SelectorError> {
        let value = value.into();
        let Some((prefix, pattern)) = value.split_once(':') else {
            return Err(SelectorError::MissingPrefix);
        };
        if !matches!(
            prefix,
            "label" | "scope" | "type" | "path" | "ref" | "title"
        ) {
            return Err(SelectorError::UnknownPrefix(prefix.to_owned()));
        }
        if pattern.is_empty() {
            return Err(SelectorError::EmptyPattern);
        }
        if prefix == "ref"
            && !(pattern.starts_with('#')
                && pattern[1..]
                    .chars()
                    .all(|character| character.is_ascii_digit())
                && !pattern[1..].starts_with('0'))
        {
            return Err(SelectorError::InvalidRef);
        }
        if prefix == "title" {
            if !pattern.contains('*') {
                return Err(SelectorError::TitleNeedsWildcard);
            }
            if pattern
                .split('*')
                .any(|literal| literal.chars().count() > 24)
            {
                return Err(SelectorError::TitleLiteralTooLong);
            }
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Selector {
    type Err = SelectorError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for Selector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SelectorError {
    #[error("selector needs a qualified prefix")]
    MissingPrefix,
    #[error("unknown selector prefix: {0}")]
    UnknownPrefix(String),
    #[error("selector value is empty")]
    EmptyPattern,
    #[error("ref selector must be ref:#N")]
    InvalidRef,
    #[error("title selector must contain *")]
    TitleNeedsWildcard,
    #[error("title selector literal run exceeds 24 characters")]
    TitleLiteralTooLong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultDisposition {
    Delegated,
    Excluded,
    Unclassified,
}

fn default_disposition() -> DefaultDisposition {
    DefaultDisposition::Unclassified
}

fn default_provider() -> String {
    "file".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMandate {
    pub repo: RepositoryName,
    #[serde(default)]
    pub paused: bool,
    #[serde(default = "default_disposition")]
    pub default: DefaultDisposition,
    #[serde(default)]
    pub delegated: Vec<Selector>,
    #[serde(default)]
    pub excluded: Vec<Selector>,
    #[serde(default)]
    pub reserved: Vec<u64>,
    #[serde(default)]
    pub bounce: Vec<Selector>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MandateConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    pub cadence_hours: u64,
    pub stuck_after_days: u64,
    #[serde(default)]
    pub search_roots: Vec<String>,
    #[serde(default)]
    pub bounce_all: Vec<Selector>,
    #[serde(default)]
    pub projects: Vec<ProjectMandate>,
}

impl MandateConfig {
    pub fn from_yaml(input: &str) -> Result<Self, ConfigError> {
        let config: Self = serde_yaml::from_str(input)?;
        config.validate()?;
        Ok(config)
    }

    pub fn to_yaml(&self) -> Result<String, serde_yaml::Error> {
        serde_yaml::to_string(self)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.provider != "file" {
            return Err(ConfigError::Invalid("provider must be file"));
        }
        if self.cadence_hours == 0 {
            return Err(ConfigError::Invalid(
                "cadence_hours must be a positive integer",
            ));
        }
        if self.search_roots.iter().any(String::is_empty) {
            return Err(ConfigError::Invalid("search roots must not be empty"));
        }
        let mut repositories = std::collections::HashSet::new();
        for project in &self.projects {
            if !repositories.insert(project.repo.as_str()) {
                return Err(ConfigError::Invalid("repository names must be unique"));
            }
            if project.reserved.contains(&0) {
                return Err(ConfigError::Invalid(
                    "reserved refs must be positive integers",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateProject {
    pub repo: RepositoryName,
    #[serde(default)]
    pub required_checks: Vec<String>,
    #[serde(default)]
    pub bounce: Vec<Selector>,
    #[serde(default)]
    pub reserved: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub bounce_all: Vec<Selector>,
    #[serde(default)]
    pub projects: Vec<GateProject>,
}

impl GateConfig {
    pub fn from_yaml(input: &str) -> Result<Self, ConfigError> {
        let config: Self = serde_yaml::from_str(input)?;
        config.validate()?;
        Ok(config)
    }

    pub fn to_yaml(&self) -> Result<String, serde_yaml::Error> {
        serde_yaml::to_string(self)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.provider != "file" {
            return Err(ConfigError::Invalid("provider must be file"));
        }
        let mut repositories = std::collections::HashSet::new();
        for project in &self.projects {
            if !repositories.insert(project.repo.as_str()) {
                return Err(ConfigError::Invalid("repository names must be unique"));
            }
            if project.required_checks.iter().any(String::is_empty) {
                return Err(ConfigError::Invalid(
                    "required check selectors must not be empty",
                ));
            }
            if project.reserved.contains(&0) {
                return Err(ConfigError::Invalid(
                    "reserved refs must be positive integers",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    Pass,
    Fail,
    Inconclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Builder,
    Gatekeeper,
    Implementer,
}

/// A mandate is classification authority, not prose explaining a decision.
///
/// In particular there is intentionally no `reason`, prompt, tool output, or
/// free-form detail field here. Narration belongs in the local trace adapter,
/// never in records handed to another store implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mandate {
    pub disposition: DefaultDisposition,
    pub selector: Option<Selector>,
}

#[cfg(test)]
mod tests {
    use super::{GateConfig, MandateConfig};

    const ROSTER: &str = r#"
provider: file
cadence_hours: 1
stuck_after_days: 7
search_roots:
  - /synthetic/repos
bounce_all:
  - title:*credential*
projects:
  - repo: example-org/example-repo
    delegated: [label:maintenance]
    excluded: []
    reserved: [42]
    default: unclassified
    paused: false
    bounce: [path:.github/workflows/**]
"#;

    const GATE: &str = r#"
provider: file
bounce_all: []
projects:
  - repo: example-org/example-repo
    required_checks: [verify-*]
    bounce: [title:*breaking API*]
    reserved: [42]
"#;

    #[test]
    fn roster_yaml_round_trips_semantically() {
        let parsed = MandateConfig::from_yaml(ROSTER).expect("synthetic roster should parse");
        let emitted = parsed.to_yaml().expect("roster should serialize");
        assert_eq!(
            parsed,
            MandateConfig::from_yaml(&emitted).expect("emitted roster should parse")
        );
    }

    #[test]
    fn gate_yaml_round_trips_semantically() {
        let parsed = GateConfig::from_yaml(GATE).expect("synthetic gate should parse");
        let emitted = parsed.to_yaml().expect("gate should serialize");
        assert_eq!(
            parsed,
            GateConfig::from_yaml(&emitted).expect("emitted gate should parse")
        );
    }
}
