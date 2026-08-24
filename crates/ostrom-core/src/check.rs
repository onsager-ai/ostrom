use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const CHECKS_VERSION: u32 = 1;
pub const RESULT_VERSION: u32 = 1;
pub const CHECK_ACTIONS: &[&str] = &[
    "agent/claude",
    "cmd/run",
    "doctor/check",
    "gh/check-run",
    "gh/token-scope",
    "http/get",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckDocument {
    pub checks_version: u32,
    #[serde(default)]
    pub inconclusive_policy: InconclusivePolicy,
    pub checks: BTreeMap<String, CheckDefinition>,
}

impl CheckDocument {
    pub fn from_yaml(input: &str) -> Result<Self, CheckContractError> {
        Self::from_yaml_with_actions(input, CHECK_ACTIONS)
    }

    pub fn from_yaml_with_actions(
        input: &str,
        actions: &[&str],
    ) -> Result<Self, CheckContractError> {
        let document: Self = serde_yaml::from_str(input).map_err(CheckContractError::Yaml)?;
        if document.checks_version != CHECKS_VERSION {
            return Err(CheckContractError::UnsupportedCheckVersion);
        }
        validate_check_definitions(&document.checks, actions)?;
        Ok(document)
    }
}

/// Validate one composed map of check definitions against an action catalogue.
pub fn validate_check_definitions(
    checks: &BTreeMap<String, CheckDefinition>,
    actions: &[&str],
) -> Result<(), CheckContractError> {
    if checks.keys().any(String::is_empty) {
        return Err(CheckContractError::EmptyCheckId);
    }
    for definition in checks.values() {
        definition.validate_uses()?;
        if !actions.contains(&definition.uses.as_str()) {
            return Err(CheckContractError::UnknownAction);
        }
        definition.validate_agent_parameters()?;
    }
    validate_local_evidence_graph(checks)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckDefinition {
    pub uses: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inconclusive_policy: Option<InconclusivePolicy>,
    pub with: BTreeMap<String, Value>,
}

impl CheckDefinition {
    fn validate_uses(&self) -> Result<(), CheckContractError> {
        let mut components = self.uses.split('/');
        if matches!(
            (components.next(), components.next(), components.next()),
            (Some(domain), Some(verb), None) if !domain.is_empty() && !verb.is_empty()
        ) {
            Ok(())
        } else {
            Err(CheckContractError::InvalidActionName)
        }
    }

    fn validate_agent_parameters(&self) -> Result<(), CheckContractError> {
        if self.uses.split_once('/').map(|part| part.0) != Some("agent") {
            return Ok(());
        }
        agent_parameters(self).map(|_| ())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceReference {
    pub from: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentParameters {
    pub prompt: String,
    pub evidence: Vec<EvidenceReference>,
    pub model: Option<String>,
}

/// Decode the core-owned `agent/*` parameters. Harnesses receive this bounded
/// input and cannot add provider-specific context keys.
pub fn agent_parameters(
    definition: &CheckDefinition,
) -> Result<AgentParameters, CheckContractError> {
    if definition.uses.split_once('/').map(|part| part.0) != Some("agent") {
        return Err(CheckContractError::InvalidAgentParameters);
    }
    let allowed = ["evidence", "fresh_for", "model", "prompt"];
    if definition
        .with
        .keys()
        .any(|key| !allowed.contains(&key.as_str()))
    {
        return Err(CheckContractError::InvalidAgentParameters);
    }
    let prompt = definition
        .with
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or(CheckContractError::InvalidAgentParameters)?
        .to_owned();
    let evidence = definition
        .with
        .get("evidence")
        .cloned()
        .ok_or(CheckContractError::JudgedEvidenceRequired)
        .and_then(|value| {
            serde_json::from_value::<Vec<EvidenceReference>>(value)
                .map_err(|_| CheckContractError::InvalidAgentParameters)
        })?;
    if evidence.is_empty() {
        return Err(CheckContractError::JudgedEvidenceRequired);
    }
    let mut names = BTreeSet::new();
    if evidence
        .iter()
        .any(|item| item.from.trim().is_empty() || !names.insert(item.from.as_str()))
    {
        return Err(CheckContractError::InvalidAgentParameters);
    }
    let model = definition
        .with
        .get("model")
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .ok_or(CheckContractError::InvalidAgentParameters)
        })
        .transpose()?;
    Ok(AgentParameters {
        prompt,
        evidence,
        model,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Catalogue {
    pub document: CheckDocument,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatalogueEnumeration {
    pub catalogues: Vec<Catalogue>,
    /// Attests that every configured source was read to completion.
    pub complete: bool,
}

fn validate_local_evidence_graph(
    checks: &BTreeMap<String, CheckDefinition>,
) -> Result<(), CheckContractError> {
    validate_evidence_cycles(checks, false)
}

fn validate_catalogue_evidence_graph(
    enumeration: &CatalogueEnumeration,
) -> Result<(), CheckContractError> {
    let mut definitions = BTreeMap::new();
    let mut duplicates = BTreeSet::new();
    for catalogue in &enumeration.catalogues {
        for (id, definition) in &catalogue.document.checks {
            if definitions.insert(id.clone(), definition.clone()).is_some() {
                duplicates.insert(id.clone());
            }
        }
    }
    for definition in definitions.values() {
        if definition.uses.starts_with("agent/") {
            for reference in agent_parameters(definition)?.evidence {
                if duplicates.contains(&reference.from) {
                    return Err(CheckContractError::AmbiguousCheck);
                }
                if !definitions.contains_key(&reference.from) {
                    return Err(CheckContractError::UnresolvedCheck);
                }
            }
        }
    }
    validate_evidence_cycles(&definitions, true)
}

fn validate_evidence_cycles(
    definitions: &BTreeMap<String, CheckDefinition>,
    require_local_references: bool,
) -> Result<(), CheckContractError> {
    fn visit(
        id: &str,
        definitions: &BTreeMap<String, CheckDefinition>,
        require_local_references: bool,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
    ) -> Result<(), CheckContractError> {
        if visited.contains(id) {
            return Ok(());
        }
        if !visiting.insert(id.to_owned()) {
            return Err(CheckContractError::EvidenceCycle);
        }
        let definition = &definitions[id];
        if definition.uses.starts_with("agent/") {
            for reference in agent_parameters(definition)?.evidence {
                if reference.from == id {
                    return Err(CheckContractError::EvidenceSelfReference);
                }
                if definitions.contains_key(&reference.from) {
                    visit(
                        &reference.from,
                        definitions,
                        require_local_references,
                        visiting,
                        visited,
                    )?;
                } else if require_local_references {
                    return Err(CheckContractError::UnresolvedCheck);
                }
            }
        }
        visiting.remove(id);
        visited.insert(id.to_owned());
        Ok(())
    }

    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for id in definitions.keys() {
        visit(
            id,
            definitions,
            require_local_references,
            &mut visiting,
            &mut visited,
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDefinition {
    pub uses: String,
    pub producer: String,
    pub default_fresh_for_seconds: u64,
    pub definition: Value,
    pub source_revision: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckBasis {
    Mechanical,
    Judged,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DefinitionDigest(String);

impl DefinitionDigest {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedCheck {
    pub id: String,
    pub definition: CheckDefinition,
    pub definition_digest: DefinitionDigest,
    pub basis: CheckBasis,
    pub producer: String,
    pub fresh_for_seconds: u64,
    pub inconclusive_policy: InconclusivePolicy,
}

#[derive(Serialize)]
struct DigestMaterial<'a> {
    catalogue_entry: CatalogueDigestMaterial<'a>,
    resolved_action: &'a ActionDefinition,
}

#[derive(Serialize)]
struct CatalogueDigestMaterial<'a> {
    id: &'a str,
    definition: &'a CheckDefinition,
    inconclusive_policy: InconclusivePolicy,
}

pub fn resolve_check(
    id: &str,
    enumeration: &CatalogueEnumeration,
    action: &ActionDefinition,
) -> Result<ResolvedCheck, CheckContractError> {
    let (definition, suite_policy) = select_check_with_policy(id, enumeration)?;
    if action.uses != definition.uses {
        return Err(CheckContractError::UnresolvedAction);
    }
    validate_action(action)?;

    let fresh_for_seconds = resolve_fresh_for(&definition.with, action.default_fresh_for_seconds)?;
    let inconclusive_policy = definition.inconclusive_policy.unwrap_or(suite_policy);
    let basis = if definition.uses.split_once('/').map(|part| part.0) == Some("agent") {
        CheckBasis::Judged
    } else {
        CheckBasis::Mechanical
    };
    let material = DigestMaterial {
        catalogue_entry: CatalogueDigestMaterial {
            id,
            definition,
            inconclusive_policy,
        },
        resolved_action: action,
    };
    let mut canonical = serde_json::to_value(&material).expect("digest material is serializable");
    canonicalize_json(&mut canonical);
    let canonical = serde_json::to_vec(&canonical).expect("canonical JSON is serializable");

    Ok(ResolvedCheck {
        id: id.to_owned(),
        definition: definition.clone(),
        definition_digest: DefinitionDigest(format!("sha256:{}", sha256_hex(&canonical))),
        basis,
        producer: action.producer.clone(),
        fresh_for_seconds,
        inconclusive_policy,
    })
}

/// Select one authored check by exact id without interpreting its opaque
/// provider parameters.
pub fn select_check<'a>(
    id: &str,
    enumeration: &'a CatalogueEnumeration,
) -> Result<&'a CheckDefinition, CheckContractError> {
    select_check_with_policy(id, enumeration).map(|(definition, _)| definition)
}

fn select_check_with_policy<'a>(
    id: &str,
    enumeration: &'a CatalogueEnumeration,
) -> Result<(&'a CheckDefinition, InconclusivePolicy), CheckContractError> {
    if !enumeration.complete {
        return Err(CheckContractError::CheckCatalogTruncated);
    }
    for catalogue in &enumeration.catalogues {
        if catalogue.document.checks_version != CHECKS_VERSION {
            return Err(CheckContractError::UnsupportedCheckVersion);
        }
        if catalogue.document.checks.keys().any(String::is_empty) {
            return Err(CheckContractError::EmptyCheckId);
        }
        for definition in catalogue.document.checks.values() {
            definition.validate_uses()?;
            definition.validate_agent_parameters()?;
        }
    }
    validate_catalogue_evidence_graph(enumeration)?;

    let mut matches = enumeration.catalogues.iter().filter_map(|catalogue| {
        catalogue
            .document
            .checks
            .get(id)
            .map(|definition| (definition, catalogue.document.inconclusive_policy))
    });
    let Some((definition, policy)) = matches.next() else {
        return Err(CheckContractError::UnresolvedCheck);
    };
    if matches.next().is_some() {
        return Err(CheckContractError::AmbiguousCheck);
    }
    Ok((definition, policy))
}

fn canonicalize_json(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(canonicalize_json),
        Value::Object(object) => {
            let previous = std::mem::take(object);
            let mut sorted = BTreeMap::new();
            for (key, mut value) in previous {
                canonicalize_json(&mut value);
                sorted.insert(key, value);
            }
            object.extend(sorted);
        }
        _ => {}
    }
}

fn validate_action(action: &ActionDefinition) -> Result<(), CheckContractError> {
    let definition = CheckDefinition {
        uses: action.uses.clone(),
        inconclusive_policy: None,
        with: BTreeMap::new(),
    };
    definition.validate_uses()?;
    if action.producer.is_empty() || action.source_revision.is_empty() {
        return Err(CheckContractError::MalformedAction);
    }
    Ok(())
}

/// Resolve the one universal key inside an otherwise opaque `with` map.
///
/// Providers still own every other parameter. Freshness accepts positive
/// integer seconds or a positive integer suffixed by `s`, `m`, `h`, `d`, or
/// `w`; keeping the representation small makes it portable across providers.
pub fn resolve_fresh_for(
    parameters: &BTreeMap<String, Value>,
    default_seconds: u64,
) -> Result<u64, FreshnessError> {
    let Some(value) = parameters.get("fresh_for") else {
        return positive_freshness(default_seconds);
    };
    match value {
        Value::Number(number) => number
            .as_u64()
            .ok_or(FreshnessError::Invalid)
            .and_then(positive_freshness),
        Value::String(value) => parse_duration(value),
        _ => Err(FreshnessError::Invalid),
    }
}

fn positive_freshness(seconds: u64) -> Result<u64, FreshnessError> {
    (seconds > 0)
        .then_some(seconds)
        .ok_or(FreshnessError::Invalid)
}

fn parse_duration(value: &str) -> Result<u64, FreshnessError> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .ok_or(FreshnessError::Invalid)?;
    let (amount, unit) = value.split_at(split);
    let amount = amount.parse::<u64>().map_err(|_| FreshnessError::Invalid)?;
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        _ => return Err(FreshnessError::Invalid),
    };
    amount
        .checked_mul(multiplier)
        .ok_or(FreshnessError::Invalid)
        .and_then(positive_freshness)
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FreshnessError {
    #[error(
        "fresh_for must be a positive integer number of seconds or a duration ending in s, m, h, d, or w"
    )]
    Invalid,
}

#[derive(Debug, Error)]
pub enum CheckContractError {
    #[error("could not parse checks YAML: {0}")]
    Yaml(serde_yaml::Error),
    #[error("unsupported_check_version")]
    UnsupportedCheckVersion,
    #[error("check id must not be empty")]
    EmptyCheckId,
    #[error("uses must have the exact shape domain/verb")]
    InvalidActionName,
    #[error("unknown_action")]
    UnknownAction,
    #[error("check_catalog_truncated")]
    CheckCatalogTruncated,
    #[error("unresolved_check")]
    UnresolvedCheck,
    #[error("ambiguous_check")]
    AmbiguousCheck,
    #[error("unresolved_action")]
    UnresolvedAction,
    #[error("resolved action metadata is malformed")]
    MalformedAction,
    #[error("invalid_agent_parameters")]
    InvalidAgentParameters,
    #[error("judged_evidence_required")]
    JudgedEvidenceRequired,
    #[error("evidence_self_reference")]
    EvidenceSelfReference,
    #[error("evidence_cycle")]
    EvidenceCycle,
    #[error("evidence_incomplete")]
    EvidenceIncomplete,
    #[error(transparent)]
    Freshness(#[from] FreshnessError),
    #[error("malformed_receipt")]
    MalformedReceipt,
}

impl CheckContractError {
    #[must_use]
    pub fn fault_name(&self) -> Option<&'static str> {
        match self {
            Self::UnsupportedCheckVersion => Some("unsupported_check_version"),
            Self::UnknownAction => Some("unknown_action"),
            Self::CheckCatalogTruncated => Some("check_catalog_truncated"),
            Self::UnresolvedCheck => Some("unresolved_check"),
            Self::AmbiguousCheck => Some("ambiguous_check"),
            Self::UnresolvedAction => Some("unresolved_action"),
            Self::InvalidAgentParameters => Some("invalid_agent_parameters"),
            Self::JudgedEvidenceRequired => Some("judged_evidence_required"),
            Self::EvidenceSelfReference => Some("evidence_self_reference"),
            Self::EvidenceCycle => Some("evidence_cycle"),
            Self::EvidenceIncomplete => Some("evidence_incomplete"),
            Self::MalformedReceipt => Some("malformed_receipt"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckVerdict {
    Pass,
    Fail,
    Inconclusive,
}

impl CheckVerdict {
    #[must_use]
    pub fn render(self, basis: CheckBasis) -> &'static str {
        match (basis, self) {
            (CheckBasis::Judged, Self::Pass) => "judged pass",
            (CheckBasis::Judged, Self::Fail) => "judged fail",
            (CheckBasis::Judged, Self::Inconclusive) => "judged inconclusive",
            (CheckBasis::Mechanical, Self::Pass) => "pass",
            (CheckBasis::Mechanical, Self::Fail) => "fail",
            (CheckBasis::Mechanical, Self::Inconclusive) => "inconclusive",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InconclusivePolicy {
    #[default]
    Block,
    Warn,
    Pass,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub name: String,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fresh_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgmentClause {
    pub evidence: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeStamp {
    pub harness: String,
    pub model: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedOutput {
    pub basis: CheckBasis,
    pub verdict: CheckVerdict,
    pub rendered: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub because: Vec<JudgmentClause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judged_by: Option<JudgeStamp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceBundleItem {
    pub name: String,
    pub digest: String,
    pub output: RecordedOutput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgmentInput {
    pub prompt: String,
    pub evidence: Vec<EvidenceBundleItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckReceipt {
    pub result_version: u32,
    pub check: String,
    pub definition_digest: DefinitionDigest,
    pub attempt_id: String,
    pub observed_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub basis: CheckBasis,
    pub producer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<CheckVerdict>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub because: Vec<JudgmentClause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judged_by: Option<JudgeStamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl CheckReceipt {
    pub fn validate(&self) -> Result<(), CheckContractError> {
        let evidence_names = self
            .evidence
            .iter()
            .map(|item| item.name.as_str())
            .collect::<BTreeSet<_>>();
        if self.result_version != RESULT_VERSION
            || self.check.is_empty()
            || self.attempt_id.is_empty()
            || self.producer.is_empty()
            || self.completed_at < self.observed_at
            || matches!(
                (self.verdict, &self.error),
                (Some(_), Some(_)) | (None, None)
            )
            || (self.verdict.is_some() && self.detail.is_some())
            || (self.error.is_some() && !self.evidence.is_empty())
            || self.error.as_ref().is_some_and(String::is_empty)
            || self
                .evidence
                .iter()
                .any(|item| item.name.is_empty() || item.digest.is_empty())
            || evidence_names.len() != self.evidence.len()
            || self
                .because
                .iter()
                .any(|clause| clause.evidence.is_empty() || clause.detail.trim().is_empty())
            || self.judged_by.as_ref().is_some_and(|judge| {
                judge.harness.is_empty() || judge.model.is_empty() || judge.version.is_empty()
            })
        {
            return Err(CheckContractError::MalformedReceipt);
        }
        match self.basis {
            CheckBasis::Mechanical => {
                if self.judged_by.is_some() || !self.because.is_empty() {
                    return Err(CheckContractError::MalformedReceipt);
                }
            }
            CheckBasis::Judged => {
                if self.judged_by.is_none()
                    || (matches!(self.verdict, Some(CheckVerdict::Pass | CheckVerdict::Fail))
                        && (self.evidence.is_empty() || self.because.is_empty()))
                    || (self.verdict == Some(CheckVerdict::Inconclusive)
                        && !self.because.is_empty())
                    || (self.error.is_some()
                        && (!self.evidence.is_empty() || !self.because.is_empty()))
                {
                    return Err(CheckContractError::MalformedReceipt);
                }
                if self
                    .because
                    .iter()
                    .any(|clause| !evidence_names.contains(clause.evidence.as_str()))
                {
                    return Err(CheckContractError::EvidenceIncomplete);
                }
            }
        }
        Ok(())
    }
}

/// Identify the exact recorded observation supplied to a judge. Attempt and
/// timing metadata are included deliberately: a re-run is a new observation
/// even when it returns the same value.
#[must_use]
pub fn receipt_digest(receipt: &CheckReceipt) -> String {
    let mut canonical = serde_json::to_value(receipt).expect("receipt serializes");
    canonicalize_json(&mut canonical);
    let bytes = serde_json::to_vec(&canonical).expect("canonical receipt serializes");
    format!("sha256:{}", sha256_hex(&bytes))
}

#[derive(Debug, Clone)]
pub struct RunnerStamp<'a> {
    pub resolved: &'a ResolvedCheck,
    pub attempt_id: &'a str,
    pub observed_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunnerResult {
    result_version: u32,
    #[serde(default)]
    verdict: Option<CheckVerdict>,
    #[serde(default)]
    evidence: Vec<Evidence>,
    #[serde(default)]
    because: Vec<JudgmentClause>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    detail: Option<String>,
}

impl RunnerStamp<'_> {
    /// Stamp a raw runner result. Runner-supplied envelope fields are removed
    /// before strict result parsing, then replaced from trusted metadata.
    pub fn stamp(self, mut result: Value) -> Result<CheckReceipt, CheckContractError> {
        let object = result
            .as_object_mut()
            .ok_or(CheckContractError::MalformedReceipt)?;
        for field in [
            "check",
            "definition_digest",
            "attempt_id",
            "observed_at",
            "completed_at",
            "basis",
            "producer",
            "judged_by",
        ] {
            object.remove(field);
        }
        let result: RunnerResult =
            serde_json::from_value(result).map_err(|_| CheckContractError::MalformedReceipt)?;
        let receipt = CheckReceipt {
            result_version: result.result_version,
            check: self.resolved.id.clone(),
            definition_digest: self.resolved.definition_digest.clone(),
            attempt_id: self.attempt_id.to_owned(),
            observed_at: self.observed_at,
            completed_at: self.completed_at,
            basis: self.resolved.basis,
            producer: self.resolved.producer.clone(),
            verdict: result.verdict,
            evidence: result.evidence,
            because: result.because,
            judged_by: None,
            error: result.error,
            detail: result.detail,
        };
        receipt.validate()?;
        Ok(receipt)
    }

    /// Stamp an executor fault, including failures that produced no runner
    /// result (for example a crash or unavailable dependency).
    pub fn fault(self, name: impl Into<String>, detail: Option<String>) -> CheckReceipt {
        CheckReceipt {
            result_version: RESULT_VERSION,
            check: self.resolved.id.clone(),
            definition_digest: self.resolved.definition_digest.clone(),
            attempt_id: self.attempt_id.to_owned(),
            observed_at: self.observed_at,
            completed_at: self.completed_at,
            basis: self.resolved.basis,
            producer: self.resolved.producer.clone(),
            verdict: None,
            evidence: Vec::new(),
            because: Vec::new(),
            judged_by: None,
            error: Some(name.into()),
            detail,
        }
    }
}

#[derive(Debug, Clone)]
pub struct JudgmentRunnerStamp<'a> {
    pub resolved: &'a ResolvedCheck,
    pub attempt_id: &'a str,
    pub observed_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub judge: JudgeStamp,
    pub evidence: Vec<Evidence>,
}

impl JudgmentRunnerStamp<'_> {
    pub fn verdict(
        self,
        verdict: CheckVerdict,
        because: Vec<JudgmentClause>,
    ) -> Result<CheckReceipt, CheckContractError> {
        let receipt = CheckReceipt {
            result_version: RESULT_VERSION,
            check: self.resolved.id.clone(),
            definition_digest: self.resolved.definition_digest.clone(),
            attempt_id: self.attempt_id.to_owned(),
            observed_at: self.observed_at,
            completed_at: self.completed_at,
            basis: self.resolved.basis,
            producer: self.resolved.producer.clone(),
            verdict: Some(verdict),
            evidence: self.evidence,
            because,
            judged_by: Some(self.judge),
            error: None,
            detail: None,
        };
        receipt.validate()?;
        Ok(receipt)
    }

    pub fn inconclusive(self) -> Result<CheckReceipt, CheckContractError> {
        let receipt = CheckReceipt {
            result_version: RESULT_VERSION,
            check: self.resolved.id.clone(),
            definition_digest: self.resolved.definition_digest.clone(),
            attempt_id: self.attempt_id.to_owned(),
            observed_at: self.observed_at,
            completed_at: self.completed_at,
            basis: self.resolved.basis,
            producer: self.resolved.producer.clone(),
            verdict: Some(CheckVerdict::Inconclusive),
            evidence: self.evidence,
            because: Vec::new(),
            judged_by: Some(self.judge),
            error: None,
            detail: None,
        };
        receipt.validate()?;
        Ok(receipt)
    }

    #[must_use]
    pub fn fault(self, name: impl Into<String>, detail: Option<String>) -> CheckReceipt {
        CheckReceipt {
            result_version: RESULT_VERSION,
            check: self.resolved.id.clone(),
            definition_digest: self.resolved.definition_digest.clone(),
            attempt_id: self.attempt_id.to_owned(),
            observed_at: self.observed_at,
            completed_at: self.completed_at,
            basis: self.resolved.basis,
            producer: self.resolved.producer.clone(),
            verdict: None,
            evidence: Vec::new(),
            because: Vec::new(),
            judged_by: Some(self.judge),
            error: Some(name.into()),
            detail,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    NeverRun,
    Stale,
    Passing,
    Failing,
    Inconclusive,
}

impl CheckState {
    #[must_use]
    pub fn render(self, basis: CheckBasis) -> &'static str {
        match (basis, self) {
            (CheckBasis::Judged, Self::Passing) => "judged pass",
            (CheckBasis::Judged, Self::Failing) => "judged fail",
            (CheckBasis::Judged, Self::Inconclusive) => "judged inconclusive",
            (CheckBasis::Judged, Self::Stale) => "judged stale",
            (CheckBasis::Judged, Self::NeverRun) => "judged never run",
            (CheckBasis::Mechanical, Self::Passing) => "pass",
            (CheckBasis::Mechanical, Self::Failing) => "fail",
            (CheckBasis::Mechanical, Self::Inconclusive) => "inconclusive",
            (CheckBasis::Mechanical, Self::Stale) => "stale",
            (CheckBasis::Mechanical, Self::NeverRun) => "never run",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckFault {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckEvaluation {
    pub state: CheckState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault: Option<CheckFault>,
}

impl ResolvedCheck {
    #[must_use]
    pub fn evaluate(
        &self,
        receipts: &[CheckReceipt],
        evaluated_at: DateTime<Utc>,
    ) -> CheckEvaluation {
        let relevant = receipts.iter().filter(|receipt| receipt.check == self.id);
        let latest_attempt = relevant.clone().max_by_key(|receipt| receipt.completed_at);
        let fault = latest_attempt.and_then(|receipt| {
            if receipt.definition_digest != self.definition_digest
                || receipt.basis != self.basis
                || receipt.producer != self.producer
            {
                Some(CheckFault {
                    name: "definition_mismatch".to_owned(),
                    detail: None,
                })
            } else if receipt.validate().is_err() {
                Some(CheckFault {
                    name: "malformed_receipt".to_owned(),
                    detail: None,
                })
            } else {
                receipt.error.as_ref().map(|name| CheckFault {
                    name: name.clone(),
                    detail: receipt.detail.clone(),
                })
            }
        });

        let latest_verdict = relevant
            .filter(|receipt| {
                receipt.definition_digest == self.definition_digest
                    && receipt.basis == self.basis
                    && receipt.producer == self.producer
                    && receipt.validate().is_ok()
                    && receipt.verdict.is_some()
            })
            .max_by_key(|receipt| receipt.observed_at);

        let state = latest_verdict.map_or(CheckState::NeverRun, |receipt| {
            let expires_at = i64::try_from(self.fresh_for_seconds)
                .ok()
                .and_then(Duration::try_seconds)
                .and_then(|duration| receipt.observed_at.checked_add_signed(duration));
            let evidence_stale = self.basis == CheckBasis::Judged
                && receipt.evidence.iter().any(|evidence| {
                    evidence_is_stale(evidence, receipts, evaluated_at, &mut BTreeSet::new())
                });
            if expires_at.is_some_and(|expires_at| expires_at < evaluated_at) || evidence_stale {
                CheckState::Stale
            } else {
                match receipt.verdict {
                    Some(CheckVerdict::Pass) => CheckState::Passing,
                    Some(CheckVerdict::Fail) => CheckState::Failing,
                    Some(CheckVerdict::Inconclusive)
                        if self.inconclusive_policy == InconclusivePolicy::Block =>
                    {
                        CheckState::Inconclusive
                    }
                    Some(CheckVerdict::Inconclusive) => CheckState::Passing,
                    None => CheckState::NeverRun,
                }
            }
        });
        CheckEvaluation { state, fault }
    }
}

fn evidence_is_stale(
    evidence: &Evidence,
    receipts: &[CheckReceipt],
    evaluated_at: DateTime<Utc>,
    visiting: &mut BTreeSet<String>,
) -> bool {
    if evidence
        .fresh_until
        .is_none_or(|fresh_until| fresh_until < evaluated_at)
        || !visiting.insert(evidence.name.clone())
    {
        return true;
    }
    let Some(receipt) = receipts
        .iter()
        .filter(|candidate| candidate.check == evidence.name)
        .max_by_key(|candidate| candidate.completed_at)
    else {
        return true;
    };
    let stale = receipt_digest(receipt) != evidence.digest
        || (receipt.basis == CheckBasis::Judged
            && receipt
                .evidence
                .iter()
                .any(|nested| evidence_is_stale(nested, receipts, evaluated_at, visiting)));
    visiting.remove(&evidence.name);
    stale
}

// Small dependency-free SHA-256 keeps the contract usable in the existing
// offline workspace. It follows FIPS 180-4 and hashes canonical JSON bytes.
pub fn sha256_hex(input: &[u8]) -> String {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = INITIAL;
    for chunk in padded.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, bytes) in chunk.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes(bytes.try_into().expect("four-byte word"));
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    state.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;

    const DOCUMENT: &str = r#"
checks_version: 1
checks:
  hub-serves-current-records:
    uses: cmd/run
    with:
      target: example-org/example-repo
      fresh_for: 1h
"#;

    fn action() -> ActionDefinition {
        ActionDefinition {
            uses: "cmd/run".to_owned(),
            producer: "test-fixture".to_owned(),
            default_fresh_for_seconds: 300,
            definition: json!({"fixture": "observation"}),
            source_revision: "fixture-action-r1".to_owned(),
        }
    }

    fn enumeration(document: CheckDocument) -> CatalogueEnumeration {
        CatalogueEnumeration {
            catalogues: vec![Catalogue { document }],
            complete: true,
        }
    }

    fn resolved() -> ResolvedCheck {
        let document = CheckDocument::from_yaml(DOCUMENT).expect("valid fixture catalogue");
        resolve_check(
            "hub-serves-current-records",
            &enumeration(document),
            &action(),
        )
        .expect("fixture check resolves")
    }

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2030, 1, 1, hour, 0, 0)
            .single()
            .expect("fixture timestamp")
    }

    fn stamp<'a>(resolved: &'a ResolvedCheck, attempt_id: &'a str, hour: u32) -> RunnerStamp<'a> {
        RunnerStamp {
            resolved,
            attempt_id,
            observed_at: at(hour),
            completed_at: at(hour),
        }
    }

    #[test]
    fn sha256_matches_the_standard_empty_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn strict_schema_rejects_unknown_and_removed_top_level_fields() {
        for field in [
            "basis: mechanical",
            "asks: []",
            "fresh_for: 1h",
            "extra: true",
        ] {
            let yaml = DOCUMENT.replace("    with:", &format!("    {field}\n    with:"));
            assert!(CheckDocument::from_yaml(&yaml).is_err(), "accepted {field}");
        }
        let unknown_document = format!("{DOCUMENT}\nunknown: true\n");
        assert!(CheckDocument::from_yaml(&unknown_document).is_err());
    }

    #[test]
    fn unknown_action_is_a_parse_failure() {
        let error = CheckDocument::from_yaml(&DOCUMENT.replace("cmd/run", "missing/observe"))
            .expect_err("the action catalogue is closed");
        assert_eq!(error.fault_name(), Some("unknown_action"));
    }

    #[test]
    fn with_is_opaque_except_for_the_reserved_freshness_override() {
        let document = CheckDocument::from_yaml(DOCUMENT).expect("opaque map should parse");
        assert_eq!(
            document.checks["hub-serves-current-records"].with["target"],
            json!("example-org/example-repo")
        );
        assert_eq!(resolved().fresh_for_seconds, 3600);
    }

    #[test]
    fn unsupported_version_has_the_named_fault() {
        let error =
            CheckDocument::from_yaml(&DOCUMENT.replace("checks_version: 1", "checks_version: 2"))
                .expect_err("future version must fail");
        assert_eq!(error.fault_name(), Some("unsupported_check_version"));
    }

    #[test]
    fn exact_resolution_rejects_ambiguity_even_for_identical_definitions() {
        let document = CheckDocument::from_yaml(DOCUMENT).expect("fixture parses");
        let mut enumeration = enumeration(document.clone());
        enumeration.catalogues.push(Catalogue { document });
        let error = resolve_check("hub-serves-current-records", &enumeration, &action())
            .expect_err("duplicate id must be ambiguous");
        assert_eq!(error.fault_name(), Some("ambiguous_check"));
        assert!(resolve_check("HUB-serves-current-records", &enumeration, &action()).is_err());
    }

    #[test]
    fn incomplete_enumeration_fails_closed_before_resolution() {
        let mut enumeration = enumeration(CheckDocument::from_yaml(DOCUMENT).expect("fixture"));
        enumeration.complete = false;
        let error = resolve_check("hub-serves-current-records", &enumeration, &action())
            .expect_err("truncated catalogue must fault");
        assert_eq!(error.fault_name(), Some("check_catalog_truncated"));
    }

    #[test]
    fn no_receipt_is_never_run_and_old_receipt_is_stale() {
        let resolved = resolved();
        assert_eq!(
            resolved.evaluate(&[], at(2)),
            CheckEvaluation {
                state: CheckState::NeverRun,
                fault: None,
            }
        );
        let receipt = stamp(&resolved, "fixture-attempt-1", 0)
            .stamp(json!({"result_version": 1, "verdict": "pass"}))
            .expect("valid receipt");
        assert_eq!(
            resolved.evaluate(&[receipt], at(2)).state,
            CheckState::Stale
        );
    }

    #[test]
    fn fresh_pass_and_fail_map_to_distinct_states() {
        let resolved = resolved();
        for (verdict, expected) in [("pass", CheckState::Passing), ("fail", CheckState::Failing)] {
            let receipt = stamp(&resolved, verdict, 0)
                .stamp(json!({"result_version": 1, "verdict": verdict}))
                .expect("valid verdict receipt");
            assert_eq!(resolved.evaluate(&[receipt], at(0)).state, expected);
        }
    }

    #[test]
    fn inconclusive_policy_defaults_to_block_and_can_be_defaulted_or_overridden() {
        let blocked = resolved();
        assert_eq!(blocked.inconclusive_policy, InconclusivePolicy::Block);
        let receipt = stamp(&blocked, "fixture-inconclusive", 0)
            .stamp(json!({"result_version": 1, "verdict": "inconclusive"}))
            .expect("valid inconclusive receipt");
        assert_eq!(
            blocked
                .evaluate(std::slice::from_ref(&receipt), at(0))
                .state,
            CheckState::Inconclusive
        );

        let suite_warn = CheckDocument::from_yaml(&DOCUMENT.replace(
            "checks_version: 1",
            "checks_version: 1\ninconclusive_policy: warn",
        ))
        .expect("suite default parses");
        let warned = resolve_check(
            "hub-serves-current-records",
            &enumeration(suite_warn),
            &action(),
        )
        .expect("suite policy resolves");
        assert_eq!(warned.inconclusive_policy, InconclusivePolicy::Warn);
        assert_ne!(blocked.definition_digest, warned.definition_digest);

        let overridden = CheckDocument::from_yaml(&DOCUMENT.replace(
            "    uses: cmd/run",
            "    uses: cmd/run\n    inconclusive_policy: pass",
        ))
        .expect("per-check policy parses");
        let passed = resolve_check(
            "hub-serves-current-records",
            &enumeration(overridden),
            &action(),
        )
        .expect("per-check policy resolves");
        assert_eq!(passed.inconclusive_policy, InconclusivePolicy::Pass);
        let receipt = stamp(&passed, "fixture-inconclusive", 0)
            .stamp(json!({"result_version": 1, "verdict": "inconclusive"}))
            .expect("valid inconclusive receipt");
        assert_eq!(
            passed.evaluate(&[receipt], at(0)).state,
            CheckState::Passing
        );
    }

    #[test]
    fn runner_cannot_assert_trusted_envelope_fields() {
        let resolved = resolved();
        let receipt = stamp(&resolved, "trusted-attempt", 0)
            .stamp(json!({
                "result_version": 1,
                "verdict": "pass",
                "basis": "judged",
                "producer": "runner-forgery",
                "check": "different-check",
                "definition_digest": "sha256:forged",
                "attempt_id": "forged-attempt",
                "observed_at": "1999-01-01T00:00:00Z",
                "completed_at": "1999-01-01T00:00:00Z"
            }))
            .expect("asserted envelope fields are overwritten");
        assert_eq!(receipt.basis, CheckBasis::Mechanical);
        assert_eq!(receipt.producer, "test-fixture");
        assert_eq!(receipt.attempt_id, "trusted-attempt");
        assert_eq!(receipt.check, "hub-serves-current-records");
    }

    #[test]
    fn verdict_and_error_together_are_malformed() {
        let resolved = resolved();
        let error = stamp(&resolved, "fixture-attempt", 0)
            .stamp(json!({
                "result_version": 1,
                "verdict": "fail",
                "error": "dependency_unavailable"
            }))
            .expect_err("runner outcome is exclusive");
        assert_eq!(error.fault_name(), Some("malformed_receipt"));
    }

    #[test]
    fn editing_a_check_retires_the_previous_verdict() {
        let old = resolved();
        let receipt = stamp(&old, "fixture-attempt", 0)
            .stamp(json!({"result_version": 1, "verdict": "pass"}))
            .expect("valid old receipt");
        let edited = CheckDocument::from_yaml(&DOCUMENT.replace(
            "target: example-org/example-repo",
            "target: example-org/edited-repo",
        ))
        .expect("edited fixture parses");
        let current = resolve_check(
            "hub-serves-current-records",
            &enumeration(edited),
            &action(),
        )
        .expect("edited check resolves");
        assert_ne!(old.definition_digest, current.definition_digest);
        let evaluation = current.evaluate(&[receipt], at(0));
        assert_eq!(evaluation.state, CheckState::NeverRun);
        assert_eq!(
            evaluation.fault.expect("mismatch is explicit").name,
            "definition_mismatch"
        );
    }

    #[test]
    fn failed_attempt_preserves_an_earlier_fresh_pass() {
        let resolved = resolved();
        let pass = stamp(&resolved, "fixture-pass", 0)
            .stamp(json!({"result_version": 1, "verdict": "pass"}))
            .expect("pass receipt");
        let crash = stamp(&resolved, "fixture-crash", 1).fault(
            "execution_error",
            Some("fixture action exited unexpectedly".to_owned()),
        );
        let evaluation = resolved.evaluate(&[pass, crash], at(1));
        assert_eq!(evaluation.state, CheckState::Passing);
        assert_eq!(
            evaluation.fault.expect("crash remains orthogonal").name,
            "execution_error"
        );
    }

    #[test]
    fn only_agent_actions_are_judged() {
        let document = CheckDocument::from_yaml(
            r#"
checks_version: 1
checks:
  source:
    uses: cmd/run
    with: {}
  hub-serves-current-records:
    uses: agent/claude
    with:
      prompt: inspect the bounded source
      evidence: [{from: source}]
"#,
        )
        .expect("agent fixture parses");
        let mut action = action();
        action.uses = "agent/claude".to_owned();
        assert_eq!(
            resolve_check(
                "hub-serves-current-records",
                &enumeration(document),
                &action,
            )
            .expect("agent action resolves")
            .basis,
            CheckBasis::Judged
        );
    }

    #[test]
    fn canonical_digest_is_stable_across_map_order() {
        let reordered = DOCUMENT.replace(
            "      target: example-org/example-repo\n      fresh_for: 1h",
            "      fresh_for: 1h\n      target: example-org/example-repo",
        );
        let first = resolved();
        let second = resolve_check(
            "hub-serves-current-records",
            &enumeration(CheckDocument::from_yaml(&reordered).expect("reordered fixture")),
            &action(),
        )
        .expect("reordered check resolves");
        assert_eq!(first.definition_digest, second.definition_digest);
    }

    #[test]
    fn canonical_digest_sorts_nested_provider_maps() {
        let first_document = CheckDocument::from_yaml(&DOCUMENT.replace(
            "target: example-org/example-repo",
            "target:\n        alpha: 1\n        beta: 2",
        ))
        .expect("first nested fixture");
        let second_document = CheckDocument::from_yaml(&DOCUMENT.replace(
            "target: example-org/example-repo",
            "target:\n        beta: 2\n        alpha: 1",
        ))
        .expect("second nested fixture");
        let first = resolve_check(
            "hub-serves-current-records",
            &enumeration(first_document),
            &action(),
        )
        .expect("first nested check resolves");
        let second = resolve_check(
            "hub-serves-current-records",
            &enumeration(second_document),
            &action(),
        )
        .expect("second nested check resolves");
        assert_eq!(first.definition_digest, second.definition_digest);
    }

    #[test]
    fn action_revision_changes_the_definition_digest() {
        let first = resolved();
        let document = CheckDocument::from_yaml(DOCUMENT).expect("fixture parses");
        let mut changed_action = action();
        changed_action.source_revision = "fixture-action-r2".to_owned();
        let second = resolve_check(
            "hub-serves-current-records",
            &enumeration(document),
            &changed_action,
        )
        .expect("changed action resolves");
        assert_ne!(first.definition_digest, second.definition_digest);
    }

    #[test]
    fn opaque_parameters_can_hold_nested_provider_specific_data() {
        let mut parameters = BTreeMap::new();
        parameters.insert("provider_payload".to_owned(), json!({"nested": [true, 42]}));
        assert_eq!(resolve_fresh_for(&parameters, 60), Ok(60));
    }

    #[test]
    fn judged_checks_require_bounded_evidence_at_authoring_time() {
        let source = r#"
checks_version: 1
checks:
  opinion:
    uses: agent/claude
    with:
      prompt: is the observation material
"#;
        let error = CheckDocument::from_yaml(source).expect_err("evidence is mandatory");
        assert_eq!(error.fault_name(), Some("judged_evidence_required"));
    }

    #[test]
    fn evidence_graph_rejects_direct_and_indirect_cycles() {
        let direct = r#"
checks_version: 1
checks:
  opinion:
    uses: agent/claude
    with:
      prompt: inspect
      evidence: [{from: opinion}]
"#;
        assert_eq!(
            CheckDocument::from_yaml(direct)
                .expect_err("self-reference must fail")
                .fault_name(),
            Some("evidence_self_reference")
        );
        let indirect = r#"
checks_version: 1
checks:
  first:
    uses: agent/claude
    with:
      prompt: inspect second
      evidence: [{from: second}]
  second:
    uses: agent/claude
    with:
      prompt: inspect first
      evidence: [{from: first}]
"#;
        assert_eq!(
            CheckDocument::from_yaml(indirect)
                .expect_err("cycle must fail")
                .fault_name(),
            Some("evidence_cycle")
        );
    }
}
