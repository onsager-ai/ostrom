use std::{collections::BTreeMap, fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::policy::{PolicyManifest, SelectorPrefix};

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

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum GateConfigDerivationError {
    #[error("gate manifest rule `{rule}` names invalid repository `{repository}`: {source}")]
    InvalidRepository {
        rule: String,
        repository: String,
        source: DomainError,
    },
    #[error("gate manifest rule `{rule}` has unsupported selector `{selector}`: {source}")]
    UnsupportedSelector {
        rule: String,
        selector: String,
        source: SelectorError,
    },
    #[error("gate manifest grant `{rule}` requires undefined check `{check}`")]
    UndefinedRequiredCheck { rule: String, check: String },
    #[error(
        "gate manifest grant `{rule}` requires check `{check}` using `{uses}`, expected `gh/check-run`"
    )]
    UnsupportedRequiredCheck {
        rule: String,
        check: String,
        uses: String,
    },
    #[error("gate manifest grant `{rule}` requires check `{check}` without a nonempty `with.name`")]
    InvalidRequiredCheckName { rule: String, check: String },
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct GateSelector(String);

impl GateSelector {
    pub fn new(value: impl Into<String>) -> Result<Self, SelectorError> {
        let value = value.into();
        if value
            .strip_prefix("substance:")
            .is_some_and(|pattern| !pattern.is_empty())
        {
            return Ok(Self(value));
        }
        Selector::new(value.clone())?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for GateSelector {
    type Err = SelectorError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for GateSelector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
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
    #[serde(default)]
    pub max_implementers_per_repository: Option<u64>,
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
    pub hold_labels: Vec<String>,
    #[serde(default)]
    pub work_ranking: Vec<String>,
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
        if self.hold_labels.iter().any(String::is_empty) {
            return Err(ConfigError::Invalid("hold labels must not be empty"));
        }
        let mut ranked_items = std::collections::HashSet::new();
        if self
            .work_ranking
            .iter()
            .any(|item| !valid_item_id(item) || !ranked_items.insert(item.as_str()))
        {
            return Err(ConfigError::Invalid(
                "work ranking must contain unique owner/repo#N item IDs",
            ));
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
            if project.max_implementers_per_repository == Some(0) {
                return Err(ConfigError::Invalid(
                    "max implementers per repository must be positive",
                ));
            }
        }
        Ok(())
    }
}

fn valid_item_id(value: &str) -> bool {
    if value.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((repository, number)) = value.rsplit_once('#') else {
        return false;
    };
    let mut parts = repository.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty()
    ) && number.starts_with(|character: char| character.is_ascii_digit() && character != '0')
        && number.chars().all(|character| character.is_ascii_digit())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateProject {
    pub repo: RepositoryName,
    #[serde(default)]
    pub required_checks: Vec<String>,
    #[serde(default)]
    pub bounce: Vec<GateSelector>,
    #[serde(default)]
    pub reserved: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub bounce_all: Vec<GateSelector>,
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

    pub fn from_manifest(manifest: &PolicyManifest) -> Result<Self, GateConfigDerivationError> {
        let mut repository_rules = BTreeMap::<String, String>::new();
        for (rule, declaration) in manifest.grants.iter().chain(&manifest.denies) {
            for repository in declaration.repositories.iter() {
                RepositoryName::new(repository.clone()).map_err(|source| {
                    GateConfigDerivationError::InvalidRepository {
                        rule: rule.clone(),
                        repository: repository.clone(),
                        source,
                    }
                })?;
                repository_rules
                    .entry(repository.clone())
                    .or_insert_with(|| rule.clone());
            }
        }

        let mut projects = repository_rules
            .keys()
            .map(|repository| {
                (
                    repository.clone(),
                    GateProject {
                        repo: RepositoryName::new(repository.clone())
                            .expect("repository names were validated"),
                        required_checks: Vec::new(),
                        bounce: Vec::new(),
                        reserved: Vec::new(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut bounce_all = Vec::new();

        for (rule, declaration) in &manifest.denies {
            if declaration.repositories.is_empty() {
                for selector in declaration.selectors.iter() {
                    push_gate_selector(&mut bounce_all, rule, selector)?;
                }
                continue;
            }
            for repository in declaration.repositories.iter() {
                let project = projects
                    .get_mut(repository)
                    .expect("every named repository has a gate project");
                for selector in declaration.selectors.iter() {
                    if selector.prefix() == SelectorPrefix::Ref {
                        let number = selector
                            .pattern()
                            .strip_prefix('#')
                            .expect("ref selectors have canonical patterns")
                            .parse::<u64>()
                            .expect("ref selectors contain validated integers");
                        if !project.reserved.contains(&number) {
                            project.reserved.push(number);
                        }
                    } else {
                        push_gate_selector(&mut project.bounce, rule, selector)?;
                    }
                }
            }
        }

        for (rule, declaration) in &manifest.grants {
            let mut names = Vec::new();
            for check in declaration.requires.iter() {
                let definition = manifest.checks.get(check).ok_or_else(|| {
                    GateConfigDerivationError::UndefinedRequiredCheck {
                        rule: rule.clone(),
                        check: check.clone(),
                    }
                })?;
                if definition.uses != "gh/check-run" {
                    return Err(GateConfigDerivationError::UnsupportedRequiredCheck {
                        rule: rule.clone(),
                        check: check.clone(),
                        uses: definition.uses.clone(),
                    });
                }
                let name = definition
                    .with
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| GateConfigDerivationError::InvalidRequiredCheckName {
                        rule: rule.clone(),
                        check: check.clone(),
                    })?;
                names.push(name.to_owned());
            }
            for repository in declaration.repositories.iter() {
                let required_checks = &mut projects
                    .get_mut(repository)
                    .expect("every named repository has a gate project")
                    .required_checks;
                for name in &names {
                    if !required_checks.contains(name) {
                        required_checks.push(name.clone());
                    }
                }
            }
        }

        Ok(Self {
            provider: default_provider(),
            bounce_all,
            projects: projects.into_values().collect(),
        })
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

fn push_gate_selector(
    destination: &mut Vec<GateSelector>,
    rule: &str,
    selector: &crate::policy::PolicySelector,
) -> Result<(), GateConfigDerivationError> {
    let rendered = selector.to_string();
    let gate_selector = GateSelector::new(rendered.clone()).map_err(|source| {
        GateConfigDerivationError::UnsupportedSelector {
            rule: rule.to_owned(),
            selector: rendered,
            source,
        }
    })?;
    if !destination.contains(&gate_selector) {
        destination.push(gate_selector);
    }
    Ok(())
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
    use crate::PolicyManifest;

    use super::{GateConfig, GateConfigDerivationError, MandateConfig, Selector, SelectorError};

    /// The legacy selector vocabulary is closed, and this is the invariant that
    /// makes `_ => false` safe in the gate and sweep matchers: both take a
    /// validated `Selector`, so an unrecognised prefix cannot reach them and be
    /// read as "evaluated, did not match". Without this test that safety is an
    /// assumption rather than a guarantee.
    #[test]
    fn the_legacy_selector_prefix_set_is_closed_and_names_the_unknown_prefix() {
        for prefix in ["check", "actor", "verb", "substance", "area", "anything"] {
            let error = Selector::new(format!("{prefix}:value"))
                .expect_err("an unknown prefix must be refused");
            assert_eq!(error, SelectorError::UnknownPrefix(prefix.to_owned()));
        }
        for accepted in [
            "label:a",
            "scope:a",
            "type:a",
            "path:a",
            "ref:#1",
            "title:*a*",
        ] {
            Selector::new(accepted).unwrap_or_else(|error| {
                panic!("`{accepted}` is part of the retired vocabulary: {error:?}")
            });
        }
    }
    const ROSTER: &str = r#"
provider: file
cadence_hours: 1
stuck_after_days: 7
search_roots:
  - /synthetic/repos
work_ranking:
  - example-org/example-repo#42
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
    fn work_ranking_rejects_duplicates_and_malformed_item_ids() {
        for ranking in [
            "  - example-org/example-repo#42\n  - example-org/example-repo#42",
            "  - example-org/example-repo#0",
            "  - example-org#42",
        ] {
            let roster = ROSTER.replace(
                "work_ranking:\n  - example-org/example-repo#42",
                &format!("work_ranking:\n{ranking}"),
            );
            assert!(
                MandateConfig::from_yaml(&roster).is_err(),
                "accepted invalid work ranking:\n{ranking}"
            );
        }
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

    #[test]
    fn translated_manifest_gate_configs_equal_their_legacy_sources() {
        let cases = [
            (
                "checks-reservations-and-global-bounce",
                r#"
provider: file
bounce_all: [label:blocked]
projects:
  - repo: placeholder-org/alpha
    required_checks: [Rust workspace, Windows compile]
    bounce: [path:protected/**]
    reserved: [41, 43]
  - repo: placeholder-org/beta
    bounce: [type:docs]
"#,
                r#"
manifest_version: 1
checks:
  rust: {uses: gh/check-run, with: {name: Rust workspace}}
  windows: {uses: gh/check-run, with: {name: Windows compile}}
grants:
  alpha-green:
    repositories: placeholder-org/alpha
    requires: [rust, windows]
  beta-without-ci:
    repositories: placeholder-org/beta
denies:
  all-blocked: {where: label:blocked}
  alpha-bounce:
    repositories: placeholder-org/alpha
    where: [path:protected/**, ref:#41, ref:43]
  beta-bounce:
    repositories: placeholder-org/beta
    where: type:docs
"#,
            ),
            (
                "repository-without-checks-or-reservations",
                r#"
provider: file
projects:
  - repo: placeholder-org/empty
"#,
                r#"
manifest_version: 1
denies:
  name-empty-repository:
    repositories: placeholder-org/empty
"#,
            ),
            (
                "global-bounces-without-projects",
                r#"
provider: file
bounce_all: [path:ostrom.yaml, type:release]
projects: []
"#,
                r#"
manifest_version: 1
denies:
  manifest-change: {where: path:ostrom.yaml}
  release-change: {where: type:release}
"#,
            ),
        ];

        for (name, gate_yaml, manifest_yaml) in cases {
            let legacy = GateConfig::from_yaml(gate_yaml)
                .unwrap_or_else(|error| panic!("{name}: legacy gate parses: {error}"));
            let manifest = PolicyManifest::from_yaml(manifest_yaml)
                .unwrap_or_else(|error| panic!("{name}: translated manifest parses: {error}"));
            let derived = GateConfig::from_manifest(&manifest)
                .unwrap_or_else(|error| panic!("{name}: gate derives: {error}"));
            assert_eq!(derived, legacy, "{name}");
        }
    }

    #[test]
    fn manifest_gate_derivation_names_unsafe_check_resolution() {
        let undefined = PolicyManifest::from_yaml(
            "manifest_version: 1\ngrants:\n  merge:\n    repositories: placeholder-org/repo\n    requires: absent\n",
        )
        .expect("manifest syntax");
        assert!(matches!(
            GateConfig::from_manifest(&undefined),
            Err(GateConfigDerivationError::UndefinedRequiredCheck { rule, check })
                if rule == "merge" && check == "absent"
        ));

        let wrong_action = PolicyManifest::from_yaml(
            "manifest_version: 1\nchecks:\n  local: {uses: cmd/run, with: {script: 'true'}}\ngrants:\n  merge:\n    repositories: placeholder-org/repo\n    requires: local\n",
        )
        .expect("manifest syntax");
        assert!(matches!(
            GateConfig::from_manifest(&wrong_action),
            Err(GateConfigDerivationError::UnsupportedRequiredCheck { rule, check, uses })
                if rule == "merge" && check == "local" && uses == "cmd/run"
        ));

        let unsupported_selector = PolicyManifest::from_yaml(
            "manifest_version: 1\ndenies:\n  actor-only: {where: actor:builder}\n",
        )
        .expect("manifest syntax");
        assert!(matches!(
            GateConfig::from_manifest(&unsupported_selector),
            Err(GateConfigDerivationError::UnsupportedSelector { rule, selector, .. })
                if rule == "actor-only" && selector == "actor:builder"
        ));
    }

    #[test]
    fn ref_deny_reserves_only_its_exact_number() {
        let manifest = PolicyManifest::from_yaml(
            "manifest_version: 1\ndenies:\n  reservation: {repositories: placeholder-org/repo, where: 'ref:199'}\n",
        )
        .expect("reservation manifest");
        let gate = GateConfig::from_manifest(&manifest).expect("gate config");
        assert_eq!(gate.projects[0].reserved, [199]);
        assert!(!gate.projects[0].reserved.contains(&19));
        assert!(gate.projects[0].bounce.is_empty());
    }
}
