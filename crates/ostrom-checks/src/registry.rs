use std::{collections::BTreeMap, path::Path, time::SystemTime};

use chrono::{DateTime, Utc};
use ostrom_core::{
    ActionDefinition, CatalogueEnumeration, CheckContractError, CheckReceipt, ResolvedCheck,
    RunnerStamp, resolve_check, select_check,
};
use serde_json::{Value, json};
use thiserror::Error;

use crate::{CommandProvider, DoctorProvider, HttpProvider};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{name}")]
pub struct ActionFault {
    name: &'static str,
    detail: Option<String>,
}

impl ActionFault {
    #[must_use]
    pub fn new(name: &'static str, detail: Option<String>) -> Self {
        Self { name, detail }
    }

    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    Pass,
    Fail,
    Error(ActionFault),
}

pub trait PreparedAction: Send {
    fn execute(&self) -> ActionOutcome;
}

/// Registration deliberately has no basis or judgment method. A provider
/// declares only an exact domain, its exact verbs, stable action metadata,
/// and provider-owned parameter preparation/execution.
pub trait ActionProvider: Send + Sync {
    fn domain(&self) -> &'static str;
    fn verbs(&self) -> &'static [&'static str];
    fn action_definition(&self, verb: &str) -> Option<ActionDefinition>;
    fn prepare(
        &self,
        verb: &str,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<Box<dyn PreparedAction>, ActionFault>;
}

#[derive(Default)]
pub struct ActionRegistry {
    providers: BTreeMap<&'static str, Box<dyn ActionProvider>>,
}

impl ActionRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct the shipped registry. The doctor script is supplied by the
    /// host because installed plugin roots are deployment-specific.
    pub fn core(doctor_script: impl AsRef<Path>) -> Result<Self, ActionFault> {
        let mut registry = Self::new();
        registry.register(HttpProvider)?;
        registry.register(CommandProvider::default())?;
        registry.register(DoctorProvider::new(doctor_script))?;
        Ok(registry)
    }

    pub fn register(&mut self, provider: impl ActionProvider + 'static) -> Result<(), ActionFault> {
        let domain = provider.domain();
        if domain == "agent" {
            return Err(ActionFault::new("judged_domain_registration", None));
        }
        if !valid_component(domain) || provider.verbs().is_empty() {
            return Err(ActionFault::new("invalid_provider_registration", None));
        }
        if self.providers.contains_key(domain) {
            return Err(ActionFault::new("ambiguous_domain", None));
        }

        let mut seen = BTreeMap::new();
        for verb in provider.verbs() {
            if !valid_component(verb) || seen.insert(*verb, ()).is_some() {
                return Err(ActionFault::new("invalid_provider_registration", None));
            }
            let Some(action) = provider.action_definition(verb) else {
                return Err(ActionFault::new("invalid_provider_registration", None));
            };
            if action.uses != format!("{domain}/{verb}")
                || action.producer.is_empty()
                || action.source_revision.is_empty()
                || action.default_fresh_for_seconds == 0
            {
                return Err(ActionFault::new("invalid_provider_registration", None));
            }
        }
        self.providers.insert(domain, Box::new(provider));
        Ok(())
    }

    pub fn prepare(
        &self,
        id: &str,
        enumeration: &CatalogueEnumeration,
    ) -> Result<PreparedCheck, ActionFault> {
        let definition = select_check(id, enumeration).map_err(contract_fault)?;
        let (domain, verb) = exact_action(&definition.uses)?;
        let provider = self
            .providers
            .get(domain)
            .ok_or_else(|| ActionFault::new("unregistered_action", None))?;
        if !provider.verbs().contains(&verb) {
            return Err(ActionFault::new("unregistered_action", None));
        }
        let action_definition = provider
            .action_definition(verb)
            .ok_or_else(|| ActionFault::new("unregistered_action", None))?;
        let action = provider.prepare(verb, &definition.with)?;
        let resolved =
            resolve_check(id, enumeration, &action_definition).map_err(contract_fault)?;
        Ok(PreparedCheck { resolved, action })
    }
}

pub struct PreparedCheck {
    resolved: ResolvedCheck,
    action: Box<dyn PreparedAction>,
}

impl PreparedCheck {
    #[must_use]
    pub fn resolved(&self) -> &ResolvedCheck {
        &self.resolved
    }

    #[must_use]
    pub fn execute(&self, attempt_id: &str) -> CheckReceipt {
        let observed_at = DateTime::<Utc>::from(SystemTime::now());
        let outcome = self.action.execute();
        let completed_at = DateTime::<Utc>::from(SystemTime::now());
        let stamp = RunnerStamp {
            resolved: &self.resolved,
            attempt_id,
            observed_at,
            completed_at,
        };
        match outcome {
            ActionOutcome::Pass => stamp
                .stamp(json!({"result_version": 1, "verdict": "pass"}))
                .expect("built-in pass result is a valid receipt"),
            ActionOutcome::Fail => stamp
                .stamp(json!({"result_version": 1, "verdict": "fail"}))
                .expect("built-in fail result is a valid receipt"),
            ActionOutcome::Error(fault) => stamp.fault(fault.name, fault.detail),
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
        return Err(ActionFault::new("unregistered_action", None));
    };
    if !valid_component(domain) || !valid_component(verb) || verb.contains('/') {
        return Err(ActionFault::new("unregistered_action", None));
    }
    Ok((domain, verb))
}

fn contract_fault(error: CheckContractError) -> ActionFault {
    ActionFault::new(
        error.fault_name().unwrap_or("invalid_check_definition"),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ostrom_core::{Catalogue, CheckDocument};

    struct FixtureProvider {
        domain: &'static str,
    }

    struct Pass;

    impl PreparedAction for Pass {
        fn execute(&self) -> ActionOutcome {
            ActionOutcome::Pass
        }
    }

    impl ActionProvider for FixtureProvider {
        fn domain(&self) -> &'static str {
            self.domain
        }

        fn verbs(&self) -> &'static [&'static str] {
            &["observe"]
        }

        fn action_definition(&self, verb: &str) -> Option<ActionDefinition> {
            (verb == "observe").then(|| ActionDefinition {
                uses: format!("{}/observe", self.domain),
                producer: "fixture-provider".to_owned(),
                default_fresh_for_seconds: 60,
                definition: json!({"fixture": true}),
                source_revision: "fixture-r1".to_owned(),
            })
        }

        fn prepare(
            &self,
            _verb: &str,
            _parameters: &BTreeMap<String, Value>,
        ) -> Result<Box<dyn PreparedAction>, ActionFault> {
            Ok(Box::new(Pass))
        }
    }

    fn catalogue(uses: &str) -> CatalogueEnumeration {
        let source = format!(
            "checks_version: 1\nchecks:\n  fixture-check:\n    uses: {uses}\n    with: {{}}\n"
        );
        CatalogueEnumeration {
            catalogues: vec![Catalogue {
                document: CheckDocument::from_yaml(&source).expect("fixture catalogue"),
            }],
            complete: true,
        }
    }

    #[test]
    fn unregistered_action_faults_by_name() {
        let error = ActionRegistry::new()
            .prepare("fixture-check", &catalogue("missing/observe"))
            .err()
            .expect("unregistered action must fault");
        assert_eq!(error.name(), "unregistered_action");
    }

    #[test]
    fn a_contested_domain_is_ambiguous_instead_of_using_precedence() {
        let mut registry = ActionRegistry::new();
        registry
            .register(FixtureProvider { domain: "fixture" })
            .expect("first owner");
        let error = registry
            .register(FixtureProvider { domain: "fixture" })
            .expect_err("second owner must be refused");
        assert_eq!(error.name(), "ambiguous_domain");
    }

    #[test]
    fn registration_cannot_reach_the_judged_domain() {
        let error = ActionRegistry::new()
            .register(FixtureProvider { domain: "agent" })
            .expect_err("agent is reserved");
        assert_eq!(error.name(), "judged_domain_registration");
    }

    #[test]
    fn registered_actions_resolve_exactly_and_remain_mechanical() {
        let mut registry = ActionRegistry::new();
        registry
            .register(FixtureProvider { domain: "fixture" })
            .expect("fixture owner");
        let prepared = registry
            .prepare("fixture-check", &catalogue("fixture/observe"))
            .expect("registered action");
        assert_eq!(
            prepared.resolved().basis,
            ostrom_core::CheckBasis::Mechanical
        );
        assert_eq!(
            prepared.execute("fixture-attempt").verdict,
            Some(ostrom_core::CheckVerdict::Pass)
        );
    }
}
