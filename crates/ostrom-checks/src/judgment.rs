use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::SystemTime,
};

use chrono::{DateTime, Duration, Utc};
use ostrom_core::{
    ActionDefinition, CatalogueEnumeration, CheckReceipt, CheckState, CheckVerdict, Evidence,
    EvidenceBundleItem, JudgeStamp, JudgmentClause, JudgmentInput, JudgmentRunnerStamp,
    RecordedOutput, ResolvedCheck, agent_parameters, receipt_digest, resolve_check, select_check,
};
use ostrom_store::{AgentRunner, Harness, PASS_MAX_TURNS, RunOutcome, RunRequest};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::ActionFault;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JudgmentOutcome {
    Verdict {
        verdict: CheckVerdict,
        because: Vec<JudgmentClause>,
    },
    Error(ActionFault),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRequest<'a> {
    pub model: &'a str,
    #[serde(flatten)]
    pub input: &'a JudgmentInput,
}

pub trait JudgmentHarness: Harness {
    fn judge(&self, request: &HarnessRequest<'_>) -> JudgmentOutcome;
}

/// JSON-stdio adapter for the registered `agent/claude` harness. The child is
/// given only the selected model, authored prompt, and bounded evidence bundle
/// on stdin; its environment is cleared before invocation.
pub struct ClaudeHarness {
    executable: PathBuf,
    version: String,
    default_model: String,
}

impl ClaudeHarness {
    #[must_use]
    pub fn new(
        executable: impl Into<PathBuf>,
        version: impl Into<String>,
        default_model: impl Into<String>,
    ) -> Self {
        Self {
            executable: executable.into(),
            version: version.into(),
            default_model: default_model.into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessResponse {
    #[serde(default)]
    verdict: Option<CheckVerdict>,
    #[serde(default)]
    because: Vec<JudgmentClause>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    detail: Option<String>,
}

impl Harness for ClaudeHarness {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }
}

impl JudgmentHarness for ClaudeHarness {
    fn judge(&self, request: &HarnessRequest<'_>) -> JudgmentOutcome {
        let mut child = match Command::new(&self.executable)
            .env_clear()
            .current_dir(self.executable.parent().unwrap_or_else(|| Path::new("/")))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                return JudgmentOutcome::Error(ActionFault::new(
                    "harness_unavailable",
                    Some(error.to_string()),
                ));
            }
        };
        let bytes = serde_json::to_vec(request).expect("harness request serializes");
        if child
            .stdin
            .take()
            .is_none_or(|mut input| input.write_all(&bytes).is_err())
        {
            return JudgmentOutcome::Error(ActionFault::new("harness_io", None));
        }
        let output = match child.wait_with_output() {
            Ok(output) => output,
            Err(error) => {
                return JudgmentOutcome::Error(ActionFault::new(
                    "harness_io",
                    Some(error.to_string()),
                ));
            }
        };
        if !output.status.success() {
            return JudgmentOutcome::Error(ActionFault::new(
                "harness_error",
                Some(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
            ));
        }
        let response: HarnessResponse = match serde_json::from_slice(&output.stdout) {
            Ok(response) => response,
            Err(error) => {
                return JudgmentOutcome::Error(ActionFault::new(
                    "harness_protocol",
                    Some(error.to_string()),
                ));
            }
        };
        match (response.verdict, response.error) {
            (Some(verdict), None) if response.detail.is_none() => JudgmentOutcome::Verdict {
                verdict,
                because: response.because,
            },
            (None, Some(error)) if response.because.is_empty() && !error.is_empty() => {
                JudgmentOutcome::Error(ActionFault::new(
                    "cannot_determine",
                    response.detail.or(Some(error)),
                ))
            }
            _ => JudgmentOutcome::Error(ActionFault::new("harness_protocol", None)),
        }
    }
}

impl AgentRunner for ClaudeHarness {
    fn run(&self, request: &RunRequest) -> RunOutcome {
        let RunRequest::Orchestrator(request) = request else {
            return RunOutcome::Error(ActionFault::new("runner_kind_mismatch", None));
        };
        let output = match fs::File::create(&request.transcript) {
            Ok(output) => output,
            Err(error) => {
                return RunOutcome::Error(ActionFault::new("runner_io", Some(error.to_string())));
            }
        };
        let error_output = match output.try_clone() {
            Ok(error_output) => error_output,
            Err(error) => {
                return RunOutcome::Error(ActionFault::new("runner_io", Some(error.to_string())));
            }
        };
        let mut command = Command::new(&self.executable);
        command
            .args([
                "--print",
                "--settings",
                &request.profile.display().to_string(),
                "--permission-mode",
                &request.permission_mode,
                "--output-format",
                "stream-json",
                "--verbose",
                "--max-turns",
                PASS_MAX_TURNS,
                &request.prompt,
            ])
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(error_output));
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return RunOutcome::Error(ActionFault::new(
                    "runner_unavailable",
                    Some(error.to_string()),
                ));
            }
        };
        match child.wait() {
            Ok(status) => RunOutcome::Exited(status),
            Err(error) => RunOutcome::Error(ActionFault::new("runner_io", Some(error.to_string()))),
        }
    }
}

#[derive(Default)]
pub struct JudgmentRegistry {
    harnesses: BTreeMap<&'static str, Arc<dyn JudgmentHarness>>,
}

impl JudgmentRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn core(claude: ClaudeHarness) -> Result<Self, ActionFault> {
        let mut registry = Self::new();
        registry.register(claude)?;
        Ok(registry)
    }

    pub fn register(&mut self, harness: impl JudgmentHarness + 'static) -> Result<(), ActionFault> {
        let name = harness.name();
        if !valid_component(name)
            || harness.version().is_empty()
            || harness.default_model().is_empty()
        {
            return Err(ActionFault::new("invalid_harness_registration", None));
        }
        if self.harnesses.contains_key(name) {
            return Err(ActionFault::new("ambiguous_harness", None));
        }
        self.harnesses.insert(name, Arc::new(harness));
        Ok(())
    }

    pub fn prepare(
        &self,
        id: &str,
        enumeration: &CatalogueEnumeration,
        resolved_evidence: &BTreeMap<String, ResolvedCheck>,
        receipts: &[CheckReceipt],
        evaluated_at: DateTime<Utc>,
    ) -> Result<PreparedJudgment, ActionFault> {
        let definition = select_check(id, enumeration).map_err(contract_fault)?;
        let (domain, harness_name) = exact_action(&definition.uses)?;
        if domain != "agent" {
            return Err(ActionFault::new("unregistered_harness", None));
        }
        let harness = self
            .harnesses
            .get(harness_name)
            .cloned()
            .ok_or_else(|| ActionFault::new("unregistered_harness", None))?;
        let parameters = agent_parameters(definition).map_err(contract_fault)?;
        let model = parameters
            .model
            .unwrap_or_else(|| harness.default_model().to_owned());
        let action = ActionDefinition {
            uses: format!("agent/{harness_name}"),
            producer: format!("agent-harness/{harness_name}"),
            default_fresh_for_seconds: 300,
            definition: json!({
                "harness": harness_name,
                "version": harness.version(),
                "default_model": harness.default_model(),
            }),
            source_revision: harness.version().to_owned(),
        };
        let resolved = resolve_check(id, enumeration, &action).map_err(contract_fault)?;
        let mut bundle = Vec::new();
        let mut evidence = Vec::new();
        for reference in parameters.evidence {
            select_check(&reference.from, enumeration).map_err(contract_fault)?;
            let source = resolved_evidence
                .get(&reference.from)
                .ok_or_else(|| ActionFault::new("unresolved_evidence", None))?;
            let receipt = receipts
                .iter()
                .filter(|receipt| receipt.check == reference.from)
                .max_by_key(|receipt| receipt.completed_at)
                .ok_or_else(|| ActionFault::new("evidence_unavailable", None))?;
            if source.evaluate(receipts, evaluated_at).state == CheckState::Stale {
                return Err(ActionFault::new("evidence_stale", None));
            }
            if receipt.definition_digest != source.definition_digest
                || receipt.basis != source.basis
                || receipt.producer != source.producer
                || receipt.validate().is_err()
            {
                return Err(ActionFault::new("evidence_unavailable", None));
            }
            let Some(verdict) = receipt.verdict else {
                return Err(ActionFault::new("evidence_unavailable", None));
            };
            let digest = receipt_digest(receipt);
            let own_fresh_until = i64::try_from(source.fresh_for_seconds)
                .ok()
                .and_then(Duration::try_seconds)
                .and_then(|duration| receipt.observed_at.checked_add_signed(duration))
                .ok_or_else(|| ActionFault::new("evidence_unavailable", None))?;
            let fresh_until = if receipt.basis == ostrom_core::CheckBasis::Judged {
                receipt
                    .evidence
                    .iter()
                    .filter_map(|item| item.fresh_until)
                    .min()
                    .map_or(own_fresh_until, |nested| nested.min(own_fresh_until))
            } else {
                own_fresh_until
            };
            bundle.push(EvidenceBundleItem {
                name: reference.from.clone(),
                digest: digest.clone(),
                output: RecordedOutput {
                    basis: receipt.basis,
                    verdict,
                    rendered: verdict.render(receipt.basis).to_owned(),
                    evidence: receipt.evidence.clone(),
                    because: receipt.because.clone(),
                    judged_by: receipt.judged_by.clone(),
                },
            });
            evidence.push(Evidence {
                name: reference.from,
                digest,
                fresh_until: Some(fresh_until),
            });
        }
        Ok(PreparedJudgment {
            resolved,
            model,
            input: JudgmentInput {
                prompt: parameters.prompt,
                evidence: bundle,
            },
            evidence,
            harness,
        })
    }
}

pub struct PreparedJudgment {
    resolved: ResolvedCheck,
    model: String,
    input: JudgmentInput,
    evidence: Vec<Evidence>,
    harness: Arc<dyn JudgmentHarness>,
}

impl PreparedJudgment {
    #[must_use]
    pub fn resolved(&self) -> &ResolvedCheck {
        &self.resolved
    }

    #[must_use]
    pub fn input(&self) -> &JudgmentInput {
        &self.input
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn execute(&self, attempt_id: &str) -> Result<CheckReceipt, ActionFault> {
        let observed_at = DateTime::<Utc>::from(SystemTime::now());
        self.execute_at(
            attempt_id,
            observed_at,
            DateTime::<Utc>::from(SystemTime::now()),
        )
    }

    pub fn execute_at(
        &self,
        attempt_id: &str,
        observed_at: DateTime<Utc>,
        completed_at: DateTime<Utc>,
    ) -> Result<CheckReceipt, ActionFault> {
        let outcome = self.harness.judge(&HarnessRequest {
            model: &self.model,
            input: &self.input,
        });
        let stamp = JudgmentRunnerStamp {
            resolved: &self.resolved,
            attempt_id,
            observed_at,
            completed_at,
            judge: JudgeStamp {
                harness: self.harness.name().to_owned(),
                model: self.model.clone(),
                version: self.harness.version().to_owned(),
            },
            evidence: self.evidence.clone(),
        };
        match outcome {
            JudgmentOutcome::Verdict { verdict, because } => {
                stamp.verdict(verdict, because).map_err(|error| {
                    ActionFault::new(error.fault_name().unwrap_or("malformed_receipt"), None)
                })
            }
            JudgmentOutcome::Error(fault) => {
                let _ = fault;
                stamp.inconclusive().map_err(|error| {
                    ActionFault::new(error.fault_name().unwrap_or("malformed_receipt"), None)
                })
            }
        }
    }
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn exact_action(uses: &str) -> Result<(&str, &str), ActionFault> {
    let Some((domain, verb)) = uses.split_once('/') else {
        return Err(ActionFault::new("unregistered_harness", None));
    };
    if !valid_component(domain) || !valid_component(verb) || verb.contains('/') {
        return Err(ActionFault::new("unregistered_harness", None));
    }
    Ok((domain, verb))
}

fn contract_fault(error: ostrom_core::CheckContractError) -> ActionFault {
    ActionFault::new(
        error.fault_name().unwrap_or("invalid_check_definition"),
        None,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{DateTime, TimeZone, Utc};
    use ostrom_core::{
        ActionDefinition, Catalogue, CheckBasis, CheckDocument, CheckEvaluation, CheckFault,
        RunnerStamp, resolve_check,
    };
    use serde_json::json;

    use super::*;

    #[derive(Clone)]
    struct FixtureHarness {
        name: &'static str,
        outcome: JudgmentOutcome,
    }

    impl Harness for FixtureHarness {
        fn name(&self) -> &'static str {
            self.name
        }

        fn version(&self) -> &str {
            "fixture-v1"
        }

        fn default_model(&self) -> &str {
            "fixture-model"
        }
    }

    impl JudgmentHarness for FixtureHarness {
        fn judge(&self, _request: &HarnessRequest<'_>) -> JudgmentOutcome {
            self.outcome.clone()
        }
    }

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2030, 1, 1, hour, 0, 0)
            .single()
            .expect("fixture time")
    }

    fn catalogue(harness: &str) -> CatalogueEnumeration {
        let source = format!(
            r#"
checks_version: 1
checks:
  observed:
    uses: fixture/observe
    with: {{}}
  material:
    uses: agent/{harness}
    with:
      prompt: is the remaining difference material
      evidence: [{{from: observed}}]
      model: fixture-opus
      fresh_for: 1d
"#
        );
        let judged = format!("agent/{harness}");
        CatalogueEnumeration {
            catalogues: vec![Catalogue {
                document: CheckDocument::from_yaml_with_actions(
                    &source,
                    &["fixture/observe", &judged],
                )
                .expect("fixture catalogue"),
            }],
            complete: true,
        }
    }

    fn mechanical(enumeration: &CatalogueEnumeration) -> ResolvedCheck {
        resolve_check(
            "observed",
            enumeration,
            &ActionDefinition {
                uses: "fixture/observe".to_owned(),
                producer: "fixture-provider".to_owned(),
                default_fresh_for_seconds: 3600,
                definition: json!({"fixture": true}),
                source_revision: "fixture-r1".to_owned(),
            },
        )
        .expect("mechanical check resolves")
    }

    fn observed_receipt(resolved: &ResolvedCheck, attempt: &str, hour: u32) -> CheckReceipt {
        RunnerStamp {
            resolved,
            attempt_id: attempt,
            observed_at: at(hour),
            completed_at: at(hour),
        }
        .stamp(json!({"result_version": 1, "verdict": "pass"}))
        .expect("mechanical receipt")
    }

    fn passing(name: &'static str, citation: &str) -> FixtureHarness {
        FixtureHarness {
            name,
            outcome: JudgmentOutcome::Verdict {
                verdict: CheckVerdict::Pass,
                because: vec![JudgmentClause {
                    evidence: citation.to_owned(),
                    detail: "the bounded observation supports the reading".to_owned(),
                }],
            },
        }
    }

    fn prepare(
        registry: &JudgmentRegistry,
        enumeration: &CatalogueEnumeration,
        receipts: &[CheckReceipt],
    ) -> PreparedJudgment {
        let source = mechanical(enumeration);
        registry
            .prepare(
                "material",
                enumeration,
                &BTreeMap::from([("observed".to_owned(), source)]),
                receipts,
                at(0),
            )
            .expect("judgment prepares")
    }

    #[test]
    fn unregistered_agent_verb_is_a_named_harness_fault() {
        let enumeration = catalogue("claude");
        let source = mechanical(&enumeration);
        let receipt = observed_receipt(&source, "observed-1", 0);
        let error = JudgmentRegistry::new()
            .prepare(
                "material",
                &enumeration,
                &BTreeMap::from([("observed".to_owned(), source)]),
                &[receipt],
                at(0),
            )
            .err()
            .expect("missing harness must fault");
        assert_eq!(error.name(), "unregistered_harness");
    }

    #[test]
    fn bundle_contains_only_named_recorded_outputs_and_model_is_executor_stamped() {
        let enumeration = catalogue("claude");
        let source = mechanical(&enumeration);
        let source_receipt = observed_receipt(&source, "observed-1", 0);
        let mut registry = JudgmentRegistry::new();
        registry
            .register(passing("claude", "observed"))
            .expect("register fixture claude harness");
        let prepared = prepare(
            &registry,
            &enumeration,
            std::slice::from_ref(&source_receipt),
        );
        assert_eq!(prepared.model(), "fixture-opus");
        assert_eq!(
            prepared.input().prompt,
            "is the remaining difference material"
        );
        assert_eq!(prepared.input().evidence.len(), 1);
        assert_eq!(prepared.input().evidence[0].name, "observed");
        assert_eq!(
            prepared.input().evidence[0].digest,
            receipt_digest(&source_receipt)
        );
        assert_eq!(
            prepared.input().evidence[0].output.verdict,
            CheckVerdict::Pass
        );
        let receipt = prepared
            .execute_at("judged-1", at(0), at(0))
            .expect("judged receipt");
        assert_eq!(receipt.basis, CheckBasis::Judged);
        assert_eq!(receipt.judged_by.expect("judge stamp").harness, "claude");
    }

    #[test]
    fn citation_outside_the_bundle_refuses_the_receipt() {
        let enumeration = catalogue("claude");
        let source = mechanical(&enumeration);
        let source_receipt = observed_receipt(&source, "observed-1", 0);
        let mut registry = JudgmentRegistry::new();
        registry
            .register(passing("claude", "not-supplied"))
            .expect("register fixture");
        let error = prepare(&registry, &enumeration, &[source_receipt])
            .execute_at("judged-1", at(0), at(0))
            .expect_err("receipt must be refused");
        assert_eq!(error.name(), "evidence_incomplete");
    }

    #[test]
    fn omitted_model_uses_and_records_the_registered_default() {
        let mut enumeration = catalogue("claude");
        enumeration.catalogues[0]
            .document
            .checks
            .get_mut("material")
            .expect("material definition")
            .with
            .remove("model");
        let source = mechanical(&enumeration);
        let source_receipt = observed_receipt(&source, "observed-1", 0);
        let mut registry = JudgmentRegistry::new();
        registry
            .register(passing("claude", "observed"))
            .expect("register fixture");
        let prepared = prepare(&registry, &enumeration, &[source_receipt]);
        assert_eq!(prepared.model(), "fixture-model");
        let receipt = prepared
            .execute_at("judged-1", at(0), at(0))
            .expect("judged receipt");
        assert_eq!(
            receipt.judged_by.expect("judge stamp").model,
            "fixture-model"
        );
    }

    #[test]
    fn stale_evidence_composes_into_judged_staleness() {
        let enumeration = catalogue("claude");
        let source = mechanical(&enumeration);
        let source_receipt = observed_receipt(&source, "observed-1", 0);
        let mut registry = JudgmentRegistry::new();
        registry
            .register(passing("claude", "observed"))
            .expect("register fixture");
        let prepared = prepare(
            &registry,
            &enumeration,
            std::slice::from_ref(&source_receipt),
        );
        let resolved = prepared.resolved().clone();
        let judgment = prepared
            .execute_at("judged-1", at(0), at(0))
            .expect("judgment receipt");
        assert_eq!(
            resolved.evaluate(&[source_receipt, judgment], at(2)).state,
            CheckState::Stale
        );
    }

    #[test]
    fn rerunning_evidence_retires_the_previous_judgment() {
        let enumeration = catalogue("claude");
        let source = mechanical(&enumeration);
        let first = observed_receipt(&source, "observed-1", 0);
        let mut registry = JudgmentRegistry::new();
        registry
            .register(passing("claude", "observed"))
            .expect("register fixture");
        let prepared = prepare(&registry, &enumeration, std::slice::from_ref(&first));
        let resolved = prepared.resolved().clone();
        let judgment = prepared
            .execute_at("judged-1", at(0), at(0))
            .expect("judgment receipt");
        let second = observed_receipt(&source, "observed-2", 1);
        assert_eq!(
            resolved.evaluate(&[first, judgment, second], at(1)).state,
            CheckState::Stale
        );
    }

    #[test]
    fn switching_harnesses_retires_the_prior_verdict() {
        let codex_catalogue = catalogue("codex");
        let source = mechanical(&codex_catalogue);
        let observed = observed_receipt(&source, "observed-1", 0);
        let mut registry = JudgmentRegistry::new();
        registry
            .register(passing("codex", "observed"))
            .expect("register codex fixture");
        registry
            .register(passing("claude", "observed"))
            .expect("register claude fixture");
        let old = prepare(&registry, &codex_catalogue, std::slice::from_ref(&observed));
        let old_receipt = old
            .execute_at("judged-codex", at(0), at(0))
            .expect("codex judgment");
        let claude_catalogue = catalogue("claude");
        let current = prepare(
            &registry,
            &claude_catalogue,
            std::slice::from_ref(&observed),
        );
        assert_ne!(
            old.resolved().definition_digest,
            current.resolved().definition_digest
        );
        assert_eq!(
            current.resolved().evaluate(&[old_receipt], at(0)),
            CheckEvaluation {
                state: CheckState::NeverRun,
                fault: Some(CheckFault {
                    name: "definition_mismatch".to_owned(),
                    detail: None,
                }),
            }
        );
    }

    #[test]
    fn cannot_determine_is_inconclusive_not_a_fail() {
        let enumeration = catalogue("claude");
        let source = mechanical(&enumeration);
        let source_receipt = observed_receipt(&source, "observed-1", 0);
        let mut registry = JudgmentRegistry::new();
        registry
            .register(FixtureHarness {
                name: "claude",
                outcome: JudgmentOutcome::Error(ActionFault::new(
                    "cannot_determine",
                    Some("the evidence is insufficient".to_owned()),
                )),
            })
            .expect("register fixture");
        let receipt = prepare(&registry, &enumeration, &[source_receipt])
            .execute_at("judged-1", at(0), at(0))
            .expect("error receipt is recorded");
        assert_eq!(receipt.verdict, Some(CheckVerdict::Inconclusive));
        assert_eq!(receipt.error, None);
        assert_eq!(receipt.basis, CheckBasis::Judged);
        assert_eq!(
            receipt.judged_by.expect("judge stamp").model,
            "fixture-opus"
        );
    }

    #[cfg(unix)]
    #[test]
    fn core_claude_harness_uses_the_bounded_json_stdio_protocol() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let fixture = tempfile::tempdir().expect("fixture directory");
        let executable = fixture.path().join("claude-fixture");
        fs::write(
            &executable,
            concat!(
                "#!/bin/sh\n",
                "input=$(cat)\n",
                "case \"$input\" in *'\"prompt\":\"is the remaining difference material\"'*) ;; *) exit 9 ;; esac\n",
                "case \"$input\" in *'\"name\":\"observed\"'*) ;; *) exit 10 ;; esac\n",
                "printf '%s\\n' '{\"verdict\":\"pass\",\"because\":[{\"evidence\":\"observed\",\"detail\":\"bounded fixture output supports the reading\"}]}'\n",
            ),
        )
        .expect("write fixture harness");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("fixture harness mode");

        let enumeration = catalogue("claude");
        let source = mechanical(&enumeration);
        let source_receipt = observed_receipt(&source, "observed-1", 0);
        let registry = JudgmentRegistry::core(ClaudeHarness::new(
            &executable,
            "claude-fixture-v1",
            "fixture-default",
        ))
        .expect("core registry");
        let receipt = prepare(&registry, &enumeration, &[source_receipt])
            .execute_at("judged-1", at(0), at(0))
            .expect("bounded fixture judgment");
        assert_eq!(receipt.verdict, Some(CheckVerdict::Pass));
        assert_eq!(receipt.because[0].evidence, "observed");
        assert_eq!(
            receipt.judged_by.expect("judge stamp").version,
            "claude-fixture-v1"
        );
    }

    #[cfg(unix)]
    #[test]
    fn claude_agent_runner_preserves_the_pass_argv_contract() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let fixture = tempfile::tempdir().expect("fixture directory");
        let executable = fixture.path().join("claude-fixture");
        fs::write(
            &executable,
            concat!("#!/bin/sh\n", "printf '%s\\n' \"$@\" > \"$0.args\"\n"),
        )
        .expect("write fixture runner");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("fixture runner mode");
        let profile = fixture.path().join("roles/builder.settings.json");
        let transcript = fixture.path().join("transcript.jsonl");
        let prompt = "Resolve the declared operation prompt.";
        let request = RunRequest::Orchestrator(ostrom_store::OrchestratorRunRequest {
            prompt: prompt.to_owned(),
            model: "fixture-model".to_owned(),
            profile: profile.clone(),
            permission_mode: "auto".to_owned(),
            ceilings: ostrom_core::ResolvedLoopCeilings::default(),
            transcript,
        });

        let outcome =
            ClaudeHarness::new(&executable, "claude-fixture-v1", "fixture-model").run(&request);
        assert!(outcome.status().is_some_and(|status| status.success()));
        let observed = fs::read_to_string(executable.with_extension("args"))
            .expect("read observed runner arguments")
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            observed,
            vec![
                "--print".to_owned(),
                "--settings".to_owned(),
                profile.display().to_string(),
                "--permission-mode".to_owned(),
                "auto".to_owned(),
                "--output-format".to_owned(),
                "stream-json".to_owned(),
                "--verbose".to_owned(),
                "--max-turns".to_owned(),
                PASS_MAX_TURNS.to_owned(),
                prompt.to_owned(),
            ]
        );
    }

    struct FixtureRunner {
        name: &'static str,
    }

    impl Harness for FixtureRunner {
        fn name(&self) -> &'static str {
            self.name
        }

        fn version(&self) -> &str {
            "fixture-v1"
        }

        fn default_model(&self) -> &str {
            "fixture-model"
        }
    }

    impl AgentRunner for FixtureRunner {
        fn run(&self, _request: &RunRequest) -> RunOutcome {
            unreachable!("registry fixture is not executed")
        }
    }

    #[test]
    fn agent_registry_resolves_named_runners_and_rejects_duplicates() {
        let mut registry = ostrom_store::AgentRegistry::core(ClaudeHarness::new(
            "claude",
            "claude-fixture-v1",
            "fixture-model",
        ))
        .expect("core agent registry");
        registry
            .register(FixtureRunner { name: "codex" })
            .expect("register second runner");

        assert_eq!(
            registry
                .get("agent/claude")
                .expect("resolve Claude runner")
                .name(),
            "claude"
        );
        assert_eq!(
            registry
                .get("agent/codex")
                .expect("resolve codex runner")
                .name(),
            "codex"
        );
        let error = registry
            .register(FixtureRunner { name: "codex" })
            .expect_err("duplicate runner must be rejected");
        assert_eq!(error.name(), "ambiguous_harness");
    }
}
