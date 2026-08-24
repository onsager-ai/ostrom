use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};
use serde_yaml::Value;
use thiserror::Error;

use crate::{
    check::{CHECK_ACTIONS, CheckDefinition, InconclusivePolicy, validate_check_definitions},
    operation::{OperationActionError, validate_operation},
};

pub const POLICY_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyManifest {
    pub manifest_version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub includes: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, InputDecl>,
    #[serde(default, skip_serializing_if = "ManifestDefaults::is_empty")]
    pub defaults: ManifestDefaults,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub actors: BTreeMap<String, ActorDecl>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checks: BTreeMap<String, CheckDefinition>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub operations: BTreeMap<String, OperationDecl>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub grants: BTreeMap<String, RuleDecl>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub denies: BTreeMap<String, RuleDecl>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub loops: BTreeMap<String, LoopDecl>,
}

impl PolicyManifest {
    pub fn parse_yaml(input: &str) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(input)
    }

    pub fn from_yaml(input: &str) -> Result<Self, ManifestError> {
        let manifest = Self::parse_yaml(input).map_err(ManifestError::Yaml)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn to_yaml(&self) -> Result<String, serde_yaml::Error> {
        serde_yaml::to_string(self)
    }

    pub fn validate(&self) -> Result<(), ManifestValidationError> {
        if self.manifest_version != POLICY_MANIFEST_VERSION {
            return Err(ManifestValidationError::ManifestVersion(
                self.manifest_version,
            ));
        }
        for (name, declaration) in &self.inputs {
            if declaration.secret && declaration.default.is_some() {
                return Err(ManifestValidationError::SecretDefault(name.clone()));
            }
            if let Some(value) = &declaration.default {
                declaration.validate_value(value).map_err(|message| {
                    ManifestValidationError::InputDefault {
                        name: name.clone(),
                        message,
                    }
                })?;
            }
        }
        validate_positive_ceiling(
            "defaults.loop",
            "concurrent",
            self.defaults.r#loop.concurrent.map(|value| value as f64),
        )?;
        validate_positive_ceiling("defaults.loop", "spend_usd", self.defaults.r#loop.spend_usd)?;
        validate_positive_ceiling(
            "defaults.loop",
            "tokens",
            self.defaults.r#loop.tokens.map(|value| value as f64),
        )?;
        validate_check_definitions(&self.checks, CHECK_ACTIONS)
            .map_err(|error| ManifestValidationError::InvalidChecks(error.to_string()))?;
        for (name, operation) in &self.operations {
            validate_operation(name, operation)?;
        }
        for (name, declaration) in &self.loops {
            self.validate_loop(name, declaration)?;
        }
        for (kind, rules) in [("grant", &self.grants), ("deny", &self.denies)] {
            for (id, rule) in rules {
                for actor in rule.actors.iter() {
                    if !self.actors.contains_key(actor) {
                        return Err(ManifestValidationError::UnknownActor {
                            kind,
                            rule: id.clone(),
                            actor: actor.clone(),
                        });
                    }
                }
                for operation in rule.operations.iter() {
                    if !self.operations.contains_key(operation) {
                        return Err(ManifestValidationError::UnknownOperation {
                            kind,
                            rule: id.clone(),
                            operation: operation.clone(),
                        });
                    }
                }
                if let Some(selector) = rule
                    .selectors
                    .iter()
                    .find(|selector| selector.references_input())
                {
                    return Err(ManifestValidationError::InputInSelector {
                        kind,
                        rule: id.clone(),
                        selector: selector.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_loop(
        &self,
        name: &str,
        declaration: &LoopDecl,
    ) -> Result<(), ManifestValidationError> {
        if !valid_policy_id(name) {
            return Err(ManifestValidationError::InvalidLoop {
                name: name.to_owned(),
                message: "name must contain only lowercase letters, digits, or `-`".to_owned(),
            });
        }
        if !self.actors.contains_key(&declaration.actor) {
            return Err(ManifestValidationError::UnknownLoopActor {
                name: name.to_owned(),
                actor: declaration.actor.clone(),
            });
        }
        if !valid_actor_id(&declaration.actor) {
            return Err(ManifestValidationError::InvalidLoop {
                name: name.to_owned(),
                message: "actor id must contain only lowercase letters, digits, `-`, or `_`"
                    .to_owned(),
            });
        }
        let operation = self.operations.get(&declaration.operation).ok_or_else(|| {
            ManifestValidationError::UnknownLoopOperation {
                name: name.to_owned(),
                operation: declaration.operation.clone(),
            }
        })?;
        for parameter in declaration.parameters.keys() {
            if !operation.params.contains_key(parameter) {
                return Err(ManifestValidationError::InvalidLoop {
                    name: name.to_owned(),
                    message: format!(
                        "parameter `{parameter}` is not declared by operation `{}`",
                        declaration.operation
                    ),
                });
            }
        }
        for (parameter, parameter_decl) in &operation.params {
            let value = declaration
                .parameters
                .get(parameter)
                .or(parameter_decl.default.as_ref());
            let Some(value) = value else {
                return Err(ManifestValidationError::InvalidLoop {
                    name: name.to_owned(),
                    message: format!("required operation parameter `{parameter}` is missing"),
                });
            };
            parameter_decl.validate_value(value).map_err(|message| {
                ManifestValidationError::InvalidLoop {
                    name: name.to_owned(),
                    message: format!("operation parameter `{parameter}` is invalid: {message}"),
                }
            })?;
        }
        validate_positive_ceiling(name, "concurrent", declaration.concurrent.map(|v| v as f64))?;
        validate_positive_ceiling(name, "spend_usd", declaration.spend_usd)?;
        validate_positive_ceiling(name, "tokens", declaration.tokens.map(|v| v as f64))?;
        for (field, value) in [
            ("cadence_hours", declaration.cadence_hours),
            ("stuck_after_days", declaration.stuck_after_days),
        ] {
            if value == Some(0) {
                return Err(ManifestValidationError::InvalidLoop {
                    name: name.to_owned(),
                    message: format!("`{field}` must be positive"),
                });
            }
        }
        Ok(())
    }

    /// Resolve one loop together with its inherited ceilings and operation defaults.
    pub fn resolve_loop(&self, name: &str) -> Result<ResolvedLoop, LoopResolutionError> {
        let declaration = self
            .loops
            .get(name)
            .ok_or_else(|| LoopResolutionError::Unknown(name.to_owned()))?;
        let operation = self.operations.get(&declaration.operation).ok_or_else(|| {
            LoopResolutionError::Invalid {
                name: name.to_owned(),
                message: format!("unknown operation `{}`", declaration.operation),
            }
        })?;
        let parameters = operation
            .params
            .iter()
            .map(|(parameter, parameter_decl)| {
                declaration
                    .parameters
                    .get(parameter)
                    .or(parameter_decl.default.as_ref())
                    .cloned()
                    .map(|value| (parameter.clone(), value))
                    .ok_or_else(|| LoopResolutionError::Invalid {
                        name: name.to_owned(),
                        message: format!("required operation parameter `{parameter}` is missing"),
                    })
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        Ok(ResolvedLoop {
            name: name.to_owned(),
            actor: declaration.actor.clone(),
            operation: declaration.operation.clone(),
            target: declaration.target.clone(),
            parameters,
            every: declaration.every.clone(),
            ceilings: ResolvedLoopCeilings {
                concurrent: declaration.concurrent.or(self.defaults.r#loop.concurrent),
                spend_usd: declaration.spend_usd.or(self.defaults.r#loop.spend_usd),
                tokens: declaration.tokens.or(self.defaults.r#loop.tokens),
            },
            publish: declaration.publish.clone(),
            cadence_hours: declaration.cadence_hours,
            stuck_after_days: declaration.stuck_after_days,
        })
    }

    pub fn resolve_inputs<F>(
        &self,
        mut environment: F,
    ) -> Result<BTreeMap<String, ResolvedInput>, InputResolutionError>
    where
        F: FnMut(&str) -> Option<String>,
    {
        self.inputs
            .iter()
            .map(|(name, declaration)| {
                declaration
                    .resolve(|variable| environment(variable))
                    .map(|resolved| (name.clone(), resolved))
                    .map_err(|source| InputResolutionError::Named {
                        name: name.clone(),
                        source: Box::new(source),
                    })
            })
            .collect()
    }

    #[must_use]
    pub fn decide(
        &self,
        actor: &str,
        operation: &str,
        candidate: &PolicyCandidate,
    ) -> PolicyDecision {
        let matching_grants = matching_rules(&self.grants, actor, operation, candidate);
        let matching_denies = matching_rules(&self.denies, actor, operation, candidate);
        PolicyDecision {
            granted: !matching_grants.is_empty() && matching_denies.is_empty(),
            matching_grants,
            matching_denies,
        }
    }

    #[must_use]
    pub fn selector_findings(&self, universe: &SelectorUniverse) -> Vec<SelectorFinding> {
        let mut findings = Vec::new();
        for (kind, rules) in [("grant", &self.grants), ("deny", &self.denies)] {
            for (rule, declaration) in rules {
                let unmatched = declaration.unmatched.unwrap_or_else(|| {
                    if kind == "grant" {
                        self.defaults.grant.unmatched
                    } else {
                        self.defaults.deny.unmatched
                    }
                });
                let repositories = if declaration.repositories.is_empty() {
                    vec![None]
                } else {
                    declaration
                        .repositories
                        .iter()
                        .map(|repository| Some(repository.as_str()))
                        .collect()
                };
                for selector in declaration.selectors.iter() {
                    for repository in &repositories {
                        match universe.validate(selector, *repository) {
                            Ok(Some(message)) => findings.push(SelectorFinding::Empty {
                                kind,
                                rule: rule.clone(),
                                selector: selector.to_string(),
                                repository: repository.map(str::to_owned),
                                unmatched,
                                message,
                            }),
                            Ok(None) => {}
                            Err(error) => findings.push(SelectorFinding::Error {
                                kind,
                                rule: rule.clone(),
                                selector: selector.to_string(),
                                repository: repository.map(str::to_owned),
                                message: error.to_string(),
                            }),
                        }
                    }
                }
            }
        }
        findings
    }
}

fn matching_rules(
    rules: &BTreeMap<String, RuleDecl>,
    actor: &str,
    operation: &str,
    candidate: &PolicyCandidate,
) -> Vec<String> {
    rules
        .iter()
        .filter(|(_, rule)| rule.matches(actor, operation, candidate))
        .map(|(id, _)| id.clone())
        .collect()
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("could not parse policy manifest: {0}")]
    Yaml(serde_yaml::Error),
    #[error(transparent)]
    Invalid(#[from] ManifestValidationError),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ManifestValidationError {
    #[error("unsupported manifest_version {0}; expected 1")]
    ManifestVersion(u32),
    #[error("secret input `{0}` may not carry a committed default")]
    SecretDefault(String),
    #[error("input `{name}` has an invalid default: {message}")]
    InputDefault { name: String, message: String },
    #[error("{kind} `{rule}` names unknown actor `{actor}`")]
    UnknownActor {
        kind: &'static str,
        rule: String,
        actor: String,
    },
    #[error("{kind} `{rule}` names unknown operation `{operation}`")]
    UnknownOperation {
        kind: &'static str,
        rule: String,
        operation: String,
    },
    #[error("{kind} `{rule}` uses input-dependent where selector `{selector}`")]
    InputInSelector {
        kind: &'static str,
        rule: String,
        selector: String,
    },
    #[error("loop `{name}` names unknown actor `{actor}`")]
    UnknownLoopActor { name: String, actor: String },
    #[error("loop `{name}` names unknown operation `{operation}`")]
    UnknownLoopOperation { name: String, operation: String },
    #[error("loop `{name}` is invalid: {message}")]
    InvalidLoop { name: String, message: String },
    #[error("checks are invalid: {0}")]
    InvalidChecks(String),
    #[error(transparent)]
    Operation(#[from] OperationActionError),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActorDecl {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationDecl {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, OperationParamDecl>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<StepDecl>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationParamDecl {
    #[serde(rename = "type")]
    pub kind: OperationParamType,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

impl OperationParamDecl {
    pub(crate) fn validate(&self) -> Result<(), String> {
        match self.kind {
            OperationParamType::Markdown | OperationParamType::Semver
                if !self.values.is_empty() =>
            {
                return Err("`values` is only valid for type enum".to_owned());
            }
            OperationParamType::Enum if self.values.is_empty() => {
                return Err("type enum requires a non-empty `values` list".to_owned());
            }
            OperationParamType::Enum => {
                let unique = self.values.iter().collect::<BTreeSet<_>>();
                if unique.len() != self.values.len() || self.values.iter().any(String::is_empty) {
                    return Err("enum values must be non-empty and unique".to_owned());
                }
            }
            OperationParamType::Markdown | OperationParamType::Semver => {}
        }
        if let Some(default) = &self.default {
            self.validate_value(default)?;
        }
        Ok(())
    }

    pub fn validate_value(&self, value: &Value) -> Result<(), String> {
        let Some(value) = value.as_str() else {
            return Err(format!("expected {} string", self.kind));
        };
        match self.kind {
            OperationParamType::Markdown => Ok(()),
            OperationParamType::Semver if valid_semver(value) => Ok(()),
            OperationParamType::Semver => Err("expected semantic version".to_owned()),
            OperationParamType::Enum if self.values.iter().any(|candidate| candidate == value) => {
                Ok(())
            }
            OperationParamType::Enum => Err("value is outside the declared enum".to_owned()),
        }
    }
}

fn valid_semver(value: &str) -> bool {
    let (without_build, build) = value
        .split_once('+')
        .map_or((value, None), |(value, build)| (value, Some(build)));
    let (core, prerelease) = without_build
        .split_once('-')
        .map_or((without_build, None), |(core, prerelease)| {
            (core, Some(prerelease))
        });
    let components = core.split('.').collect::<Vec<_>>();
    components.len() == 3
        && components.iter().all(|component| {
            !component.is_empty()
                && component.bytes().all(|byte| byte.is_ascii_digit())
                && (component == &"0" || !component.starts_with('0'))
        })
        && prerelease.is_none_or(|value| valid_semver_identifiers(value, true))
        && build.is_none_or(|value| valid_semver_identifiers(value, false))
}

fn valid_semver_identifiers(value: &str, numeric_leading_zero_forbidden: bool) -> bool {
    !value.is_empty()
        && value.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && (!numeric_leading_zero_forbidden
                    || !identifier.bytes().all(|byte| byte.is_ascii_digit())
                    || identifier == "0"
                    || !identifier.starts_with('0'))
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationParamType {
    Markdown,
    Semver,
    Enum,
}

impl fmt::Display for OperationParamType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Markdown => "markdown",
            Self::Semver => "semver",
            Self::Enum => "enum",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepDecl {
    pub uses: String,
    #[serde(default, rename = "with", skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "NormalizedList::is_empty")]
    pub requires: NormalizedList<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopDecl {
    pub actor: String,
    pub operation: String,
    pub target: String,
    pub every: LoopCadence,
    #[serde(default, rename = "with", skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrent: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cadence_hours: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck_after_days: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopCadence {
    Hourly,
    Minute(u8),
    Times(Vec<LoopTime>),
    Range { start: LoopTime, end: LoopTime },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopActivationSlot {
    pub identity: String,
    pub age: chrono::Duration,
}

impl LoopCadence {
    #[must_use]
    pub fn on_calendars(&self) -> Vec<String> {
        match self {
            Self::Hourly => vec!["hourly".to_owned()],
            Self::Minute(minute) => vec![format!("*-*-* *:{minute:02}:00")],
            Self::Times(times) => {
                if times.iter().all(|time| time.minute == times[0].minute) {
                    let hours = times
                        .iter()
                        .map(|time| format!("{:02}", time.hour))
                        .collect::<Vec<_>>()
                        .join(",");
                    vec![format!("*-*-* {hours}:{:02}:00", times[0].minute)]
                } else {
                    times
                        .iter()
                        .map(|time| format!("*-*-* {time}:00"))
                        .collect()
                }
            }
            Self::Range { start, end } => vec![format!(
                "*-*-* {:02}..{:02}:{:02}:00",
                start.hour, end.hour, start.minute
            )],
        }
    }

    /// Return the most recent scheduled occurrence at or before `now`.
    ///
    /// The caller supplies the timezone because authored loop times are civil
    /// times. The reconciler passes the host's local time, matching the
    /// existing systemd `OnCalendar` behavior, while tests can inject any
    /// explicit timezone without consulting the wall clock.
    #[must_use]
    pub fn activation_slot<Tz>(&self, now: &chrono::DateTime<Tz>) -> Option<LoopActivationSlot>
    where
        Tz: chrono::TimeZone,
        Tz::Offset: std::fmt::Display,
    {
        use chrono::LocalResult;

        let today = now.date_naive();
        let yesterday = today.pred_opt();
        let mut latest = None;
        for date in [Some(today), yesterday].into_iter().flatten() {
            for time in self.scheduled_times() {
                let Some(local) = date.and_hms_opt(u32::from(time.hour), u32::from(time.minute), 0)
                else {
                    continue;
                };
                let candidates = match now.timezone().from_local_datetime(&local) {
                    LocalResult::Single(candidate) => vec![candidate],
                    LocalResult::Ambiguous(earlier, later) => vec![earlier, later],
                    LocalResult::None => Vec::new(),
                };
                for candidate in candidates {
                    if candidate.timestamp_millis() <= now.timestamp_millis()
                        && latest
                            .as_ref()
                            .is_none_or(|current: &chrono::DateTime<Tz>| {
                                candidate.timestamp_millis() > current.timestamp_millis()
                            })
                    {
                        latest = Some(candidate);
                    }
                }
            }
        }
        latest.map(|scheduled| LoopActivationSlot {
            identity: scheduled.format("%Y-%m-%dT%H:%M%:z").to_string(),
            age: now.naive_utc().signed_duration_since(scheduled.naive_utc()),
        })
    }

    fn scheduled_times(&self) -> Vec<LoopTime> {
        match self {
            Self::Hourly => (0..24).map(|hour| LoopTime { hour, minute: 0 }).collect(),
            Self::Minute(minute) => (0..24)
                .map(|hour| LoopTime {
                    hour,
                    minute: *minute,
                })
                .collect(),
            Self::Times(times) => times.clone(),
            Self::Range { start, end } => (start.hour..=end.hour)
                .map(|hour| LoopTime {
                    hour,
                    minute: start.minute,
                })
                .collect(),
        }
    }
}

impl Serialize for LoopCadence {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Hourly => serializer.serialize_str("hourly"),
            Self::Minute(minute) => serializer.serialize_str(&format!("*:{minute:02}")),
            Self::Times(times) => times
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .serialize(serializer),
            Self::Range { start, end } => serializer.serialize_str(&format!("{start}..{end}")),
        }
    }
}

impl<'de> Deserialize<'de> for LoopCadence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        parse_loop_cadence(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LoopTime {
    hour: u8,
    minute: u8,
}

impl fmt::Display for LoopTime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:02}:{:02}", self.hour, self.minute)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedLoop {
    pub name: String,
    pub actor: String,
    pub operation: String,
    pub target: String,
    pub parameters: BTreeMap<String, Value>,
    pub every: LoopCadence,
    pub ceilings: ResolvedLoopCeilings,
    pub publish: Option<String>,
    pub cadence_hours: Option<u64>,
    pub stuck_after_days: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ResolvedLoopCeilings {
    pub concurrent: Option<u64>,
    pub spend_usd: Option<f64>,
    pub tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LoopResolutionError {
    #[error("unknown loop `{0}`")]
    Unknown(String),
    #[error("loop `{name}` is invalid: {message}")]
    Invalid { name: String, message: String },
}

fn parse_loop_cadence(value: Value) -> Result<LoopCadence, String> {
    match value {
        Value::String(value) if value == "hourly" => Ok(LoopCadence::Hourly),
        Value::String(value) => parse_cadence_string(&value),
        Value::Sequence(values) if !values.is_empty() => {
            let times = values
                .into_iter()
                .map(|value| {
                    value
                        .as_str()
                        .ok_or_else(|| "time lists may contain only HH:MM strings".to_owned())
                        .and_then(parse_loop_time)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let unique = times.iter().collect::<BTreeSet<_>>();
            if unique.len() != times.len() {
                return Err("time lists may not contain duplicates".to_owned());
            }
            Ok(LoopCadence::Times(times))
        }
        _ => Err(
            "expected `hourly`, `*:MM`, `HH:MM..HH:MM`, or a non-empty list of HH:MM times"
                .to_owned(),
        ),
    }
}

fn parse_cadence_string(value: &str) -> Result<LoopCadence, String> {
    if let Some(minute) = value.strip_prefix("*:") {
        return parse_minute(minute).map(LoopCadence::Minute);
    }
    if let Some((start, end)) = value.split_once("..") {
        let start = parse_loop_time(start)?;
        let end = parse_loop_time(end)?;
        if start.hour > end.hour || start.minute != end.minute {
            return Err("a time range must advance in hours at one fixed minute".to_owned());
        }
        return Ok(LoopCadence::Range { start, end });
    }
    Err(format!(
        "unsupported named cadence or time expression `{value}`"
    ))
}

fn parse_loop_time(value: &str) -> Result<LoopTime, String> {
    let Some((hour, minute)) = value.split_once(':') else {
        return Err(format!("invalid loop time `{value}`; expected HH:MM"));
    };
    if hour.len() != 2 || minute.len() != 2 {
        return Err(format!("invalid loop time `{value}`; expected HH:MM"));
    }
    let hour = hour
        .parse::<u8>()
        .ok()
        .filter(|hour| *hour < 24)
        .ok_or_else(|| format!("invalid loop hour in `{value}`"))?;
    let minute = parse_minute(minute)?;
    Ok(LoopTime { hour, minute })
}

fn parse_minute(value: &str) -> Result<u8, String> {
    if value.len() != 2 {
        return Err(format!(
            "invalid loop minute `{value}`; expected two digits"
        ));
    }
    value
        .parse::<u8>()
        .ok()
        .filter(|minute| *minute < 60)
        .ok_or_else(|| format!("invalid loop minute `{value}`"))
}

fn valid_policy_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_actor_id(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn validate_positive_ceiling(
    name: &str,
    field: &str,
    value: Option<f64>,
) -> Result<(), ManifestValidationError> {
    if value.is_some_and(|value| !value.is_finite() || value <= 0.0) {
        return Err(ManifestValidationError::InvalidLoop {
            name: name.to_owned(),
            message: format!("`{field}` must be finite and positive"),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestDefaults {
    #[serde(
        default = "default_stalls_after",
        skip_serializing_if = "is_default_stalls_after"
    )]
    pub stalls_after: StallDuration,
    #[serde(default, skip_serializing_if = "LoopDefaults::is_empty")]
    pub r#loop: LoopDefaults,
    #[serde(default, skip_serializing_if = "CheckDefaults::is_empty")]
    pub check: CheckDefaults,
    #[serde(default, skip_serializing_if = "RuleDefaults::is_grant_default")]
    pub grant: RuleDefaults,
    #[serde(
        default = "RuleDefaults::deny",
        skip_serializing_if = "RuleDefaults::is_deny_default"
    )]
    pub deny: RuleDefaults,
}

impl Default for ManifestDefaults {
    fn default() -> Self {
        Self {
            stalls_after: default_stalls_after(),
            r#loop: LoopDefaults::default(),
            check: CheckDefaults::default(),
            grant: RuleDefaults::default(),
            deny: RuleDefaults::deny(),
        }
    }
}

impl ManifestDefaults {
    fn is_empty(&self) -> bool {
        is_default_stalls_after(&self.stalls_after)
            && self.r#loop.is_empty()
            && self.check.is_empty()
            && self.grant.is_grant_default()
            && self.deny.is_deny_default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckDefaults {
    #[serde(default, skip_serializing_if = "is_block_inconclusive_policy")]
    pub inconclusive_policy: InconclusivePolicy,
}

impl CheckDefaults {
    fn is_empty(&self) -> bool {
        is_block_inconclusive_policy(&self.inconclusive_policy)
    }
}

fn is_block_inconclusive_policy(policy: &InconclusivePolicy) -> bool {
    *policy == InconclusivePolicy::Block
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StallDuration {
    value: String,
    seconds: u64,
}

impl StallDuration {
    #[must_use]
    pub const fn as_seconds(&self) -> u64 {
        self.seconds
    }
}

impl Default for StallDuration {
    fn default() -> Self {
        default_stalls_after()
    }
}

impl fmt::Display for StallDuration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.value)
    }
}

impl FromStr for StallDuration {
    type Err = StallDurationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let split = value
            .find(|character: char| !character.is_ascii_digit())
            .ok_or(StallDurationError)?;
        let (amount, unit) = value.split_at(split);
        let amount = amount.parse::<u64>().map_err(|_| StallDurationError)?;
        let multiplier = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3_600,
            "d" => 86_400,
            "w" => 604_800,
            _ => return Err(StallDurationError),
        };
        let seconds = amount
            .checked_mul(multiplier)
            .filter(|seconds| *seconds > 0)
            .ok_or(StallDurationError)?;
        Ok(Self {
            value: value.to_owned(),
            seconds,
        })
    }
}

impl Serialize for StallDuration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.value)
    }
}

impl<'de> Deserialize<'de> for StallDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("stalls_after must be a positive duration such as 7d")]
pub struct StallDurationError;

fn default_stalls_after() -> StallDuration {
    StallDuration {
        value: "7d".to_owned(),
        seconds: 7 * 86_400,
    }
}

fn is_default_stalls_after(value: &StallDuration) -> bool {
    value == &default_stalls_after()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrent: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
}

impl LoopDefaults {
    fn is_empty(&self) -> bool {
        self.concurrent.is_none() && self.spend_usd.is_none() && self.tokens.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnmatchedPolicy {
    Block,
    Warn,
    Pass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleDefaults {
    #[serde(default = "default_grant_unmatched")]
    pub unmatched: UnmatchedPolicy,
}

impl RuleDefaults {
    fn deny() -> Self {
        Self {
            unmatched: UnmatchedPolicy::Block,
        }
    }

    fn is_grant_default(&self) -> bool {
        self.unmatched == UnmatchedPolicy::Warn
    }

    fn is_deny_default(&self) -> bool {
        self.unmatched == UnmatchedPolicy::Block
    }
}

impl Default for RuleDefaults {
    fn default() -> Self {
        Self {
            unmatched: default_grant_unmatched(),
        }
    }
}

const fn default_grant_unmatched() -> UnmatchedPolicy {
    UnmatchedPolicy::Warn
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleDecl {
    #[serde(default, skip_serializing_if = "NormalizedList::is_empty")]
    pub actors: NormalizedList<String>,
    #[serde(default, skip_serializing_if = "NormalizedList::is_empty")]
    pub operations: NormalizedList<String>,
    #[serde(default, skip_serializing_if = "NormalizedList::is_empty")]
    pub repositories: NormalizedList<String>,
    #[serde(
        default,
        rename = "where",
        skip_serializing_if = "NormalizedList::is_empty"
    )]
    pub selectors: NormalizedList<PolicySelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unmatched: Option<UnmatchedPolicy>,
    #[serde(default, skip_serializing_if = "NormalizedList::is_empty")]
    pub requires: NormalizedList<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalls_after: Option<StallDuration>,
}

impl RuleDecl {
    #[must_use]
    pub fn matches(&self, actor: &str, operation: &str, candidate: &PolicyCandidate) -> bool {
        dimension_matches(&self.actors, actor)
            && dimension_matches(&self.operations, operation)
            && dimension_matches(&self.repositories, &candidate.repository)
            && (self.selectors.is_empty()
                || self
                    .selectors
                    .iter()
                    .any(|selector| selector.matches(candidate)))
    }
}

fn dimension_matches(values: &NormalizedList<String>, candidate: &str) -> bool {
    values.is_empty() || values.iter().any(|value| value == candidate)
}

/// Scalar-or-list input normalized into exactly one in-memory form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedList<T>(Vec<T>);

impl<T> Default for NormalizedList<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> NormalizedList<T> {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.0.iter()
    }
}

impl<T> From<Vec<T>> for NormalizedList<T> {
    fn from(values: Vec<T>) -> Self {
        Self(values)
    }
}

impl<T> Serialize for NormalizedList<T>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de, T> Deserialize<'de> for NormalizedList<T>
where
    T: DeserializeOwned,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let values = match value {
            Value::Sequence(values) => values
                .into_iter()
                .map(serde_yaml::from_value)
                .collect::<Result<Vec<_>, _>>(),
            scalar => serde_yaml::from_value(scalar).map(|value| vec![value]),
        };
        values.map(Self).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputDecl {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<InputType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub secret: bool,
}

impl InputDecl {
    pub fn resolve<F>(&self, mut environment: F) -> Result<ResolvedInput, InputResolutionError>
    where
        F: FnMut(&str) -> Option<String>,
    {
        let value = if let Some(raw) = self.env.as_deref().and_then(&mut environment) {
            Some(self.parse_environment(&raw)?)
        } else {
            self.default.clone()
        };
        if let Some(value) = &value {
            self.validate_value(value)
                .map_err(InputResolutionError::Value)?;
        }
        Ok(ResolvedInput {
            value,
            secret: self.secret,
        })
    }

    fn parse_environment(&self, raw: &str) -> Result<Value, InputResolutionError> {
        let parsed = match self.kind.unwrap_or(InputType::String) {
            InputType::String => Value::String(raw.to_owned()),
            InputType::Integer => raw
                .parse::<i64>()
                .map(Value::from)
                .map_err(|_| InputResolutionError::EnvironmentType(InputType::Integer))?,
            InputType::Number => raw
                .parse::<f64>()
                .map(Value::from)
                .map_err(|_| InputResolutionError::EnvironmentType(InputType::Number))?,
            InputType::Boolean => raw
                .parse::<bool>()
                .map(Value::from)
                .map_err(|_| InputResolutionError::EnvironmentType(InputType::Boolean))?,
            kind @ (InputType::Array | InputType::Object) => {
                let value: Value = serde_yaml::from_str(raw)
                    .map_err(|_| InputResolutionError::EnvironmentType(kind))?;
                value
            }
        };
        Ok(parsed)
    }

    fn validate_value(&self, value: &Value) -> Result<(), String> {
        let Some(kind) = self.kind else {
            return Ok(());
        };
        let valid = match kind {
            InputType::String => value.is_string(),
            InputType::Integer => value.as_i64().is_some() || value.as_u64().is_some(),
            InputType::Number => value.is_number(),
            InputType::Boolean => value.is_bool(),
            InputType::Array => value.is_sequence(),
            InputType::Object => value.is_mapping(),
        };
        if valid {
            Ok(())
        } else {
            Err(format!("expected {kind}"))
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputType {
    String,
    Integer,
    Number,
    Boolean,
    Array,
    Object,
}

impl fmt::Display for InputType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Array => "array",
            Self::Object => "object",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedInput {
    pub value: Option<Value>,
    pub secret: bool,
}

impl ResolvedInput {
    #[must_use]
    pub fn masked_value(&self) -> Option<Value> {
        self.value.as_ref().map(|value| {
            if self.secret {
                Value::String("<secret>".to_owned())
            } else {
                value.clone()
            }
        })
    }
}

#[derive(Debug, Error)]
pub enum InputResolutionError {
    #[error("input `{name}`: {source}")]
    Named { name: String, source: Box<Self> },
    #[error("environment value does not have declared type {0}")]
    EnvironmentType(InputType),
    #[error("resolved value is invalid: {0}")]
    Value(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SelectorPrefix {
    Label,
    Path,
    Ref,
    Type,
    Actor,
    Verb,
}

impl SelectorPrefix {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Label => "label",
            Self::Path => "path",
            Self::Ref => "ref",
            Self::Type => "type",
            Self::Actor => "actor",
            Self::Verb => "verb",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PolicySelector {
    prefix: SelectorPrefix,
    pattern: String,
}

impl PolicySelector {
    pub fn new(value: impl Into<String>) -> Result<Self, PolicySelectorError> {
        let value = value.into();
        let Some((prefix, pattern)) = value.split_once(':') else {
            return Err(PolicySelectorError::MissingPrefix);
        };
        let prefix = match prefix {
            "label" => SelectorPrefix::Label,
            "path" => SelectorPrefix::Path,
            "ref" => SelectorPrefix::Ref,
            "type" => SelectorPrefix::Type,
            "actor" => SelectorPrefix::Actor,
            "verb" => SelectorPrefix::Verb,
            unknown => return Err(PolicySelectorError::UnknownPrefix(unknown.to_owned())),
        };
        if pattern.is_empty() {
            return Err(PolicySelectorError::EmptyPattern);
        }
        if pattern.chars().any(char::is_control) {
            return Err(PolicySelectorError::InvalidPattern(pattern.to_owned()));
        }
        let pattern = if prefix == SelectorPrefix::Ref {
            let number = pattern.strip_prefix('#').unwrap_or(pattern);
            if number.is_empty()
                || !number.bytes().all(|byte| byte.is_ascii_digit())
                || number.starts_with('0')
                || number.parse::<u64>().is_err()
            {
                return Err(PolicySelectorError::InvalidRef(pattern.to_owned()));
            }
            format!("#{number}")
        } else {
            pattern.to_owned()
        };
        Ok(Self { prefix, pattern })
    }

    #[must_use]
    pub const fn prefix(&self) -> SelectorPrefix {
        self.prefix
    }

    #[must_use]
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    fn references_input(&self) -> bool {
        self.pattern.contains("$inputs.")
            || self.pattern.contains("${{ inputs.")
            || self.pattern.contains("${inputs.")
    }

    #[must_use]
    pub fn matches(&self, candidate: &PolicyCandidate) -> bool {
        match self.prefix {
            SelectorPrefix::Label => candidate
                .labels
                .iter()
                .any(|label| glob_matches(label, &self.pattern, false)),
            SelectorPrefix::Path => candidate
                .paths
                .iter()
                .any(|path| glob_matches(path, &self.pattern, true)),
            SelectorPrefix::Ref => candidate
                .reference
                .is_some_and(|reference| self.pattern == format!("#{reference}")),
            SelectorPrefix::Type => candidate
                .commit_type
                .as_deref()
                .is_some_and(|kind| glob_matches(kind, &self.pattern, false)),
            SelectorPrefix::Actor => candidate
                .actor
                .as_deref()
                .is_some_and(|actor| glob_matches(actor, &self.pattern, false)),
            SelectorPrefix::Verb => candidate
                .verb
                .as_deref()
                .is_some_and(|verb| glob_matches(verb, &self.pattern, false)),
        }
    }
}

impl fmt::Display for PolicySelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.prefix.as_str(), self.pattern)
    }
}

impl Serialize for PolicySelector {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PolicySelector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PolicySelectorError {
    #[error("selector needs a qualified prefix")]
    MissingPrefix,
    #[error("unknown selector prefix: {0}")]
    UnknownPrefix(String),
    #[error("selector value is empty")]
    EmptyPattern,
    #[error("selector pattern contains a control character: {0:?}")]
    InvalidPattern(String),
    #[error("ref selector must name one positive integer without globbing: {0}")]
    InvalidRef(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyCandidate {
    pub repository: String,
    pub reference: Option<u64>,
    pub labels: Vec<String>,
    pub paths: Vec<String>,
    pub commit_type: Option<String>,
    pub actor: Option<String>,
    pub verb: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDecision {
    pub granted: bool,
    pub matching_grants: Vec<String>,
    pub matching_denies: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectorUniverse {
    pub labels: BTreeMap<String, BTreeSet<String>>,
    pub paths: BTreeMap<String, BTreeSet<String>>,
    pub actors: BTreeSet<String>,
    pub verbs: BTreeSet<String>,
}

impl SelectorUniverse {
    pub fn from_manifest(
        manifest: &PolicyManifest,
        verbs: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            actors: manifest.actors.keys().cloned().collect(),
            verbs: verbs.into_iter().collect(),
            ..Self::default()
        }
    }

    /// An error is structurally unresolvable. `Some` is a non-blocking
    /// empty-match finding. Missing repository snapshots are skipped offline.
    pub fn validate(
        &self,
        selector: &PolicySelector,
        repository: Option<&str>,
    ) -> Result<Option<String>, SelectorResolutionError> {
        match selector.prefix {
            SelectorPrefix::Label => {
                let Some(repository) = repository else {
                    return Ok(None);
                };
                let Some(labels) = self.labels.get(repository) else {
                    return Ok(None);
                };
                if labels
                    .iter()
                    .any(|label| glob_matches(label, &selector.pattern, false))
                {
                    Ok(None)
                } else {
                    Err(SelectorResolutionError::UnknownLabel {
                        repository: repository.to_owned(),
                        pattern: selector.pattern.clone(),
                    })
                }
            }
            SelectorPrefix::Path => {
                let Some(repository) = repository else {
                    return Ok(None);
                };
                let Some(paths) = self.paths.get(repository) else {
                    return Ok(None);
                };
                if paths
                    .iter()
                    .any(|path| glob_matches(path, &selector.pattern, true))
                {
                    Ok(None)
                } else {
                    Ok(Some(format!(
                        "path selector matches no file in `{repository}`"
                    )))
                }
            }
            SelectorPrefix::Ref => Ok(None),
            SelectorPrefix::Type => {
                if CONVENTIONAL_TYPES
                    .iter()
                    .any(|kind| glob_matches(kind, &selector.pattern, false))
                {
                    Ok(None)
                } else {
                    Err(SelectorResolutionError::UnknownType(
                        selector.pattern.clone(),
                    ))
                }
            }
            SelectorPrefix::Actor => resolve_named(&self.actors, selector)
                .map(|()| None)
                .map_err(|()| SelectorResolutionError::UnknownActor(selector.pattern.clone())),
            SelectorPrefix::Verb => resolve_named(&self.verbs, selector)
                .map(|()| None)
                .map_err(|()| SelectorResolutionError::UnknownVerb(selector.pattern.clone())),
        }
    }

    pub fn resolve(
        &self,
        selector: &PolicySelector,
        candidate: &PolicyCandidate,
    ) -> Result<SelectorMatch, SelectorResolutionError> {
        let _ = self.validate(selector, Some(&candidate.repository))?;
        Ok(if selector.matches(candidate) {
            SelectorMatch::Matched
        } else {
            SelectorMatch::Empty
        })
    }
}

fn resolve_named(names: &BTreeSet<String>, selector: &PolicySelector) -> Result<(), ()> {
    names
        .iter()
        .any(|name| glob_matches(name, &selector.pattern, false))
        .then_some(())
        .ok_or(())
}

const CONVENTIONAL_TYPES: [&str; 13] = [
    "build",
    "chore",
    "ci",
    "docs",
    "feat",
    "fix",
    "marketing",
    "perf",
    "refactor",
    "release",
    "revert",
    "style",
    "test",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorMatch {
    Matched,
    Empty,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SelectorResolutionError {
    #[error("label `{pattern}` does not exist in repository `{repository}`")]
    UnknownLabel { repository: String, pattern: String },
    #[error("type selector matches no conventional-commit type: {0}")]
    UnknownType(String),
    #[error("actor selector matches no declared actor: {0}")]
    UnknownActor(String),
    #[error("verb selector matches no Ostrom command: {0}")]
    UnknownVerb(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "severity", rename_all = "lowercase")]
pub enum SelectorFinding {
    Error {
        kind: &'static str,
        rule: String,
        selector: String,
        repository: Option<String>,
        message: String,
    },
    Empty {
        kind: &'static str,
        rule: String,
        selector: String,
        repository: Option<String>,
        unmatched: UnmatchedPolicy,
        message: String,
    },
}

impl SelectorFinding {
    #[must_use]
    pub const fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }
}

fn glob_matches(value: &str, pattern: &str, path: bool) -> bool {
    let value = value.to_lowercase().chars().collect::<Vec<_>>();
    let pattern = pattern.to_lowercase().chars().collect::<Vec<_>>();
    let mut memo = BTreeMap::new();
    glob_matches_at(&value, &pattern, path, 0, 0, &mut memo)
}

fn glob_matches_at(
    value: &[char],
    pattern: &[char],
    path: bool,
    value_index: usize,
    pattern_index: usize,
    memo: &mut BTreeMap<(usize, usize), bool>,
) -> bool {
    if let Some(result) = memo.get(&(value_index, pattern_index)) {
        return *result;
    }
    let result = if pattern_index == pattern.len() {
        value_index == value.len()
    } else if pattern[pattern_index] == '*' {
        let double = path && pattern.get(pattern_index + 1) == Some(&'*');
        let next_pattern = pattern_index + usize::from(double) + 1;
        let skip_pattern = if double && pattern.get(next_pattern) == Some(&'/') {
            next_pattern + 1
        } else {
            next_pattern
        };
        glob_matches_at(value, pattern, path, value_index, skip_pattern, memo)
            || (value_index < value.len()
                && (double || !path || value[value_index] != '/')
                && glob_matches_at(value, pattern, path, value_index + 1, pattern_index, memo))
    } else {
        value_index < value.len()
            && pattern[pattern_index] == value[value_index]
            && glob_matches_at(
                value,
                pattern,
                path,
                value_index + 1,
                pattern_index + 1,
                memo,
            )
    };
    memo.insert((value_index, pattern_index), result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
manifest_version: 1
actors:
  builder: {}
  gatekeeper: {}
operations:
  work: {steps: []}
  merge: {steps: []}
grants:
  delegated:
    actors: builder
    operations: [work, merge]
    repositories: placeholder-org/alpha
    where: label:area:schema
denies:
  builder-cannot-merge:
    actors: builder
    operations: merge
"#;

    fn candidate(labels: &[&str]) -> PolicyCandidate {
        PolicyCandidate {
            repository: "placeholder-org/alpha".to_owned(),
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            ..PolicyCandidate::default()
        }
    }

    #[test]
    fn scalar_fields_normalize_and_deny_wins() {
        let manifest = PolicyManifest::from_yaml(BASE).expect("manifest parses");
        assert!(
            manifest
                .decide("builder", "work", &candidate(&["area:schema"]))
                .granted
        );
        let merge = manifest.decide("builder", "merge", &candidate(&["area:schema"]));
        assert!(!merge.granted);
        assert_eq!(merge.matching_denies, ["builder-cannot-merge"]);

        let normalized = manifest.to_yaml().expect("normalizes");
        assert!(
            normalized.contains("actors:\n    - builder"),
            "{normalized}"
        );
        assert!(
            normalized.contains("where:\n    - label:area:schema"),
            "{normalized}"
        );
    }

    #[test]
    fn every_grant_and_deny_permutation_has_the_same_decision() {
        let header =
            "manifest_version: 1\nactors: {builder: {}}\noperations: {work: {steps: []}}\n";
        let grants = [
            "  grant-label: {actors: builder, operations: work, where: label:area:schema}\n",
            "  grant-type: {actors: builder, operations: work, where: type:docs}\n",
            "  grant-path: {actors: builder, operations: work, where: path:protected/**}\n",
        ];
        let denies = [
            "  deny-label: {actors: builder, operations: work, where: label:area:schema}\n",
            "  deny-type: {actors: builder, operations: work, where: type:docs}\n",
            "  deny-path: {actors: builder, operations: work, where: path:protected/**}\n",
        ];
        let permutations = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let subject = PolicyCandidate {
            paths: vec!["protected/file.txt".to_owned()],
            commit_type: Some("docs".to_owned()),
            ..candidate(&["area:schema"])
        };
        let mut expected = None;
        for grant_order in permutations {
            for deny_order in permutations {
                let yaml = format!(
                    "{header}grants:\n{}{}{}denies:\n{}{}{}",
                    grants[grant_order[0]],
                    grants[grant_order[1]],
                    grants[grant_order[2]],
                    denies[deny_order[0]],
                    denies[deny_order[1]],
                    denies[deny_order[2]],
                );
                let decision = PolicyManifest::from_yaml(&yaml)
                    .expect("permutation parses")
                    .decide("builder", "work", &subject);
                assert!(!decision.granted);
                if let Some(expected) = &expected {
                    assert_eq!(&decision, expected);
                } else {
                    expected = Some(decision);
                }
            }
        }
    }

    #[test]
    fn unknown_keys_at_nested_levels_name_the_key() {
        let cases = [
            ("manifest_version: 1\nextra: nope\n", "extra"),
            (
                "manifest_version: 1\nactors:\n  builder: {nickname: nope}\n",
                "nickname",
            ),
            (
                "manifest_version: 1\ninputs:\n  value: {type: string, override: nope}\n",
                "override",
            ),
            (
                "manifest_version: 1\noperations:\n  work:\n    steps:\n      - uses: cmd/run\n        extra: nope\n",
                "extra",
            ),
            (
                "manifest_version: 1\nactors: {builder: {}}\noperations: {work: {steps: []}}\ngrants:\n  work: {actors: builder, operations: work, extra: nope}\n",
                "extra",
            ),
            (
                "manifest_version: 1\nloops:\n  sweep: {actor: sweeper, operation: sweep, target: placeholder-org/repo, every: hourly, extra: nope}\n",
                "extra",
            ),
            (
                "manifest_version: 1\ndefaults:\n  grant: {unmatched: warn, extra: nope}\n",
                "extra",
            ),
            (
                "manifest_version: 1\ndefaults:\n  check: {inconclusive_policy: block, extra: nope}\n",
                "extra",
            ),
        ];
        for (yaml, key) in cases {
            let error = PolicyManifest::from_yaml(yaml)
                .expect_err("unknown key fails")
                .to_string();
            assert!(error.contains(key), "{error}");
        }
    }

    #[test]
    fn loop_cadence_grammar_is_closed_and_renders_systemd_calendar_values() {
        let prefix = "manifest_version: 1\nactors: {builder: {}}\noperations: {work: {steps: []}}\ngrants:\n  work: {actors: builder, operations: work, repositories: placeholder-org/repo}\nloops:\n  cadence:\n    actor: builder\n    operation: work\n    target: placeholder-org/repo\n    every: ";
        for (value, expected) in [
            ("hourly\n", "hourly"),
            ("'*:45'\n", "*-*-* *:45:00"),
            ("08:15..21:15\n", "*-*-* 08..21:15:00"),
            ("['23:15', '02:15', '05:15']\n", "*-*-* 23,02,05:15:00"),
        ] {
            let manifest = PolicyManifest::from_yaml(&format!("{prefix}{value}"))
                .expect("closed cadence value parses");
            assert_eq!(manifest.loops["cadence"].every.on_calendars(), [expected]);
        }
        for value in ["'*/15 * * * *'\n", "daily\n", "'24:00..25:00'\n", "[]\n"] {
            let error = PolicyManifest::from_yaml(&format!("{prefix}{value}"))
                .expect_err("outside cadence grammar fails")
                .to_string();
            assert!(
                error.contains("cadence") || error.contains("loop"),
                "{error}"
            );
        }
    }

    #[test]
    fn loop_cadence_identifies_the_most_recent_civil_time_slot() {
        let declared = chrono::DateTime::parse_from_rfc3339("2026-08-24T08:16:00+08:00")
            .expect("fixed civil time");
        let due = PolicyManifest::from_yaml(
            "manifest_version: 1\nactors: {builder: {}}\noperations: {work: {steps: []}}\ngrants:\n  work: {actors: builder, operations: work, repositories: placeholder-org/repo}\nloops:\n  due: {actor: builder, operation: work, target: placeholder-org/repo, every: '08:15..21:15'}\n",
        )
        .expect("loop manifest")
        .resolve_loop("due")
        .expect("resolved loop");

        let after_declared = due
            .every
            .activation_slot(&declared)
            .expect("the earlier declared minute is the current slot");
        assert_eq!(after_declared.identity, "2026-08-24T08:15+08:00");
        assert_eq!(after_declared.age, chrono::Duration::minutes(1));

        let same_slot = declared + chrono::Duration::minutes(43);
        assert_eq!(
            due.every
                .activation_slot(&same_slot)
                .expect("same slot")
                .identity,
            after_declared.identity
        );

        let next_slot = declared + chrono::Duration::minutes(59);
        assert_eq!(
            due.every
                .activation_slot(&next_slot)
                .expect("next slot")
                .identity,
            "2026-08-24T09:15+08:00"
        );
    }

    #[test]
    fn loop_ceilings_inherit_independently_and_can_override_one_value() {
        let manifest = PolicyManifest::from_yaml(
            "manifest_version: 1\ndefaults:\n  loop: {concurrent: 6, spend_usd: 50, tokens: 200000}\nactors: {builder: {}}\noperations: {work: {steps: []}}\ngrants:\n  work: {actors: builder, operations: work, repositories: placeholder-org/repo}\nloops:\n  day: {actor: builder, operation: work, target: placeholder-org/repo, every: hourly}\n  night: {actor: builder, operation: work, target: placeholder-org/repo, every: hourly, concurrent: 2}\n",
        )
        .expect("loop manifest");
        let day = manifest.resolve_loop("day").expect("day loop");
        let night = manifest.resolve_loop("night").expect("night loop");
        assert_eq!(day.ceilings.concurrent, Some(6));
        assert_eq!(day.ceilings.spend_usd, Some(50.0));
        assert_eq!(day.ceilings.tokens, Some(200_000));
        assert_eq!(night.ceilings.concurrent, Some(2));
        assert_eq!(night.ceilings.spend_usd, day.ceilings.spend_usd);
        assert_eq!(night.ceilings.tokens, day.ceilings.tokens);
    }

    #[test]
    fn step_requires_is_plural_and_closed_schema() {
        let manifest = PolicyManifest::from_yaml(
            "manifest_version: 1\noperations:\n  merge:\n    steps:\n      - uses: gh/merge-pr\n        requires: placeholder-ci\n      - uses: gh/merge-pr\n        requires: [first-ci, second-ci]\n",
        )
        .expect("plural requirement parses");
        assert_eq!(
            manifest.operations["merge"].steps[0]
                .requires
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["placeholder-ci"]
        );
        assert_eq!(
            manifest.operations["merge"].steps[1]
                .requires
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["first-ci", "second-ci"]
        );

        let error = PolicyManifest::from_yaml(
            "manifest_version: 1\noperations:\n  merge:\n    steps:\n      - uses: gh/merge-pr\n        require: placeholder-ci\n",
        )
        .expect_err("singular requirement is not in the schema")
        .to_string();
        assert!(error.contains("require"), "{error}");
    }

    #[test]
    fn unknown_version_refuses_to_load() {
        let error =
            PolicyManifest::from_yaml("manifest_version: 2\n").expect_err("future version fails");
        assert!(error.to_string().contains("manifest_version 2"));
    }

    #[test]
    fn unmatched_defaults_warn_for_grants_and_block_for_denies() {
        for yaml in [
            "manifest_version: 1\n",
            "manifest_version: 1\ndefaults: {}\n",
        ] {
            let manifest = PolicyManifest::from_yaml(yaml).expect("defaults parse");
            assert_eq!(manifest.defaults.grant.unmatched, UnmatchedPolicy::Warn);
            assert_eq!(manifest.defaults.deny.unmatched, UnmatchedPolicy::Block);
            assert_eq!(
                manifest.defaults.check.inconclusive_policy,
                InconclusivePolicy::Block
            );
            assert_eq!(manifest.defaults.stalls_after.to_string(), "7d");
            assert_eq!(manifest.defaults.stalls_after.as_seconds(), 604_800);
        }

        let manifest = PolicyManifest::from_yaml(
            "manifest_version: 1\ndefaults: {check: {inconclusive_policy: warn}}\n",
        )
        .expect("check default parses");
        assert_eq!(
            manifest.defaults.check.inconclusive_policy,
            InconclusivePolicy::Warn
        );
    }

    #[test]
    fn stalls_after_defaults_to_seven_days_and_rules_override_it() {
        let manifest = PolicyManifest::from_yaml(
            "manifest_version: 1\ndefaults: {stalls_after: 9d}\ngrants:\n  held-placeholder:\n    requires: placeholder-check\n    stalls_after: 12h\n",
        )
        .expect("stalls_after durations parse");
        assert_eq!(manifest.defaults.stalls_after.to_string(), "9d");
        assert_eq!(
            manifest.grants["held-placeholder"]
                .stalls_after
                .as_ref()
                .map(StallDuration::as_seconds),
            Some(43_200)
        );
        assert_eq!(
            manifest.grants["held-placeholder"]
                .requires
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["placeholder-check"]
        );

        for value in ["0d", "7", "-1d", "1day", "18446744073709551615w"] {
            let error = PolicyManifest::from_yaml(&format!(
                "manifest_version: 1\ndefaults: {{stalls_after: {value}}}\n"
            ))
            .expect_err("invalid stalls_after must fail");
            assert!(error.to_string().contains("stalls_after"), "{error}");
        }
    }

    #[test]
    fn grant_requires_is_plural_and_closed_schema() {
        let manifest = PolicyManifest::from_yaml(
            "manifest_version: 1\ngrants:\n  scalar:\n    requires: placeholder-check\n  sequence:\n    requires: [first-check, second-check]\n",
        )
        .expect("plural grant requirement parses");
        assert_eq!(
            manifest.grants["scalar"]
                .requires
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["placeholder-check"]
        );
        assert_eq!(
            manifest.grants["sequence"]
                .requires
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["first-check", "second-check"]
        );
        let error = PolicyManifest::from_yaml(
            "manifest_version: 1\ngrants:\n  placeholder:\n    require: placeholder-check\n",
        )
        .expect_err("singular grant requirement is not in the schema");
        assert!(error.to_string().contains("require"), "{error}");
    }

    #[test]
    fn input_env_precedes_default_and_secret_is_masked() {
        let manifest = PolicyManifest::from_yaml(
            "manifest_version: 1\ninputs:\n  count: {type: integer, env: COUNT, default: 2}\n  token: {type: string, env: TOKEN, secret: true}\n",
        )
        .expect("inputs parse");
        let resolved = manifest
            .resolve_inputs(|name| match name {
                "COUNT" => Some("7".to_owned()),
                "TOKEN" => Some("placeholder-secret".to_owned()),
                _ => None,
            })
            .expect("inputs resolve");
        assert_eq!(resolved["count"].value, Some(Value::from(7)));
        assert_eq!(
            resolved["token"].masked_value(),
            Some(Value::String("<secret>".to_owned()))
        );
    }

    #[test]
    fn secret_default_fails_validation() {
        let error = PolicyManifest::from_yaml(
            "manifest_version: 1\ninputs:\n  token: {type: string, secret: true, default: placeholder}\n",
        )
        .expect_err("secret default fails");
        assert!(error.to_string().contains("token"));
        assert!(error.to_string().contains("committed default"));
    }

    #[test]
    fn input_in_where_fails_validation() {
        let yaml = BASE.replace("label:area:schema", "label:$inputs.area");
        let error = PolicyManifest::from_yaml(&yaml).expect_err("input selector fails");
        assert!(error.to_string().contains("input-dependent where"));
        assert!(error.to_string().contains("$inputs.area"));
    }

    #[test]
    fn selector_prefix_set_is_closed_and_names_unknown_prefix() {
        for prefix in ["title", "scope", "check", "anything"] {
            let error =
                PolicySelector::new(format!("{prefix}:value")).expect_err("prefix must fail");
            assert_eq!(error, PolicySelectorError::UnknownPrefix(prefix.to_owned()));
            assert!(error.to_string().contains(prefix));
        }
    }

    #[test]
    fn ref_selectors_are_exact_positive_integers_in_hash_form() {
        let hash = PolicySelector::new("ref:#199").expect("hash form");
        let bare = PolicySelector::new("ref:199").expect("bare form");
        assert_eq!(hash, bare);
        assert_eq!(hash.to_string(), "ref:#199");
        assert_eq!(
            SelectorUniverse::default()
                .validate(&hash, None)
                .expect("ref validation is offline-safe"),
            None
        );
        assert!(hash.matches(&PolicyCandidate {
            reference: Some(199),
            ..PolicyCandidate::default()
        }));
        assert!(!hash.matches(&PolicyCandidate {
            reference: Some(19),
            ..PolicyCandidate::default()
        }));
        for invalid in ["ref:*", "ref:19*", "ref:0", "ref:#0"] {
            let error = PolicySelector::new(invalid).expect_err("fuzzy ref must fail");
            assert!(matches!(error, PolicySelectorError::InvalidRef(_)));
            assert!(error.to_string().contains("without globbing"), "{error}");
        }
    }

    #[test]
    fn label_absence_is_error_but_path_candidate_absence_is_empty() {
        let mut universe = SelectorUniverse::default();
        universe.labels.insert(
            "placeholder-org/alpha".to_owned(),
            BTreeSet::from(["area:schema".to_owned()]),
        );
        universe.paths.insert(
            "placeholder-org/alpha".to_owned(),
            BTreeSet::from(["docs/guide.md".to_owned()]),
        );
        let missing = PolicySelector::new("label:not-present").expect("valid syntax");
        assert!(matches!(
            universe.validate(&missing, Some("placeholder-org/alpha")),
            Err(SelectorResolutionError::UnknownLabel { .. })
        ));

        let path = PolicySelector::new("path:docs/**").expect("valid syntax");
        assert_eq!(
            universe
                .resolve(&path, &candidate(&[]))
                .expect("path is resolvable"),
            SelectorMatch::Empty
        );
    }

    #[test]
    fn actor_type_and_verb_resolvers_use_closed_universes() {
        let manifest = PolicyManifest::from_yaml(BASE).expect("manifest parses");
        let universe =
            SelectorUniverse::from_manifest(&manifest, ["validate".to_owned(), "merge".to_owned()]);
        for selector in ["actor:builder", "type:feat", "verb:validate"] {
            assert_eq!(
                universe
                    .validate(
                        &PolicySelector::new(selector).expect("syntax"),
                        Some("placeholder-org/alpha")
                    )
                    .expect("resolves"),
                None
            );
        }
        assert!(
            universe
                .validate(
                    &PolicySelector::new("verb:not-a-command").expect("syntax"),
                    None
                )
                .is_err()
        );
    }

    #[test]
    fn glob_path_double_star_spans_directories() {
        assert!(glob_matches(
            "crates/core/src/lib.rs",
            "crates/**/lib.rs",
            true
        ));
        assert!(glob_matches("docs/guide.md", "docs/**", true));
        assert!(glob_matches(".env", "**/.env", true));
        assert!(!glob_matches("src/docs/guide.md", "docs/*", true));
    }

    #[test]
    fn dead_tools_selector_is_a_named_non_blocking_finding() {
        let manifest = PolicyManifest::from_yaml(
            "manifest_version: 1\nactors: {builder: {}}\noperations: {work: {steps: []}}\ngrants:\n  retired-tools:\n    actors: builder\n    operations: work\n    repositories: placeholder-org/alpha\n    where: path:**/*-tools*\n",
        )
        .expect("manifest parses");
        let mut universe = SelectorUniverse::from_manifest(&manifest, ["validate".to_owned()]);
        universe.paths.insert(
            "placeholder-org/alpha".to_owned(),
            BTreeSet::from(["src/lib.rs".to_owned()]),
        );
        let findings = manifest.selector_findings(&universe);
        assert_eq!(findings.len(), 1);
        assert!(!findings[0].is_error());
        assert!(matches!(
            findings[0],
            SelectorFinding::Empty {
                unmatched: UnmatchedPolicy::Warn,
                ..
            }
        ));
        assert!(format!("{:?}", findings[0]).contains("retired-tools"));
        assert!(format!("{:?}", findings[0]).contains("*-tools"));
    }
}
