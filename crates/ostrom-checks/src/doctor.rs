use std::{collections::BTreeMap, path::PathBuf, process::Command};

use ostrom_core::ActionDefinition;
use serde_json::{Value, json};

use crate::{
    ActionFault, ActionOutcome, ActionProvider, PreparedAction,
    process::{ProcessResult, exact_keys, invalid_parameters, parameter_timeout, run_bounded},
};

pub const DOCTOR_CHECKS: &[&str] = &[
    "plugin",
    "marketplace",
    "plugin-cache-drift",
    "rules-layers",
    "touch-durability",
    "provider-reachable",
    "dispatch-source-roots",
    "trace-lease",
    "work-orders",
    "builder-pass",
    "gatekeeper-pass",
    "publish",
    "environment",
    "config-parser",
];

pub struct DoctorProvider {
    runtime: PathBuf,
    script: PathBuf,
}

impl DoctorProvider {
    #[must_use]
    pub fn new(script: impl AsRef<std::path::Path>) -> Self {
        Self::with_runtime("node", script)
    }

    #[must_use]
    pub fn with_runtime(runtime: impl Into<PathBuf>, script: impl AsRef<std::path::Path>) -> Self {
        Self {
            runtime: runtime.into(),
            script: script.as_ref().to_owned(),
        }
    }
}

impl ActionProvider for DoctorProvider {
    fn domain(&self) -> &'static str {
        "doctor"
    }

    fn verbs(&self) -> &'static [&'static str] {
        &["check"]
    }

    fn action_definition(&self, verb: &str) -> Option<ActionDefinition> {
        (verb == "check").then(|| ActionDefinition {
            uses: "doctor/check".to_owned(),
            producer: "ostrom-doctor".to_owned(),
            default_fresh_for_seconds: 300,
            definition: json!({
                "checks": DOCTOR_CHECKS,
                "parameters": ["check", "timeout"],
                "protocol": "one STATUS|name|detail|remedy line"
            }),
            source_revision: "doctor-check-v1".to_owned(),
        })
    }

    fn prepare(
        &self,
        verb: &str,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<Box<dyn PreparedAction>, ActionFault> {
        if verb != "check" || !exact_keys(parameters, &["check", "timeout"]) {
            return Err(invalid_parameters());
        }
        let check = parameters
            .get("check")
            .and_then(Value::as_str)
            .filter(|name| DOCTOR_CHECKS.contains(name))
            .ok_or_else(|| ActionFault::new("doctor_unknown_check", None))?;
        let timeout = parameter_timeout(parameters.get("timeout"))?;
        Ok(Box::new(DoctorCheck {
            runtime: self.runtime.clone(),
            script: self.script.clone(),
            check: check.to_owned(),
            timeout,
        }))
    }
}

struct DoctorCheck {
    runtime: PathBuf,
    script: PathBuf,
    check: String,
    timeout: std::time::Duration,
}

impl PreparedAction for DoctorCheck {
    fn execute(&self) -> ActionOutcome {
        let mut command = Command::new(&self.runtime);
        command.arg(&self.script).arg("--check").arg(&self.check);
        match run_bounded(&mut command, self.timeout, true) {
            ProcessResult::Completed(status, Some(output)) if status.success() => {
                parse_result(&output, &self.check)
            }
            ProcessResult::TimedOut => {
                ActionOutcome::Error(ActionFault::new("doctor_timeout", None))
            }
            ProcessResult::Completed(_, _)
            | ProcessResult::SpawnFailed
            | ProcessResult::WaitFailed
            | ProcessResult::OutputMalformed => {
                ActionOutcome::Error(ActionFault::new("doctor_execute_error", None))
            }
        }
    }
}

fn parse_result(output: &str, expected_name: &str) -> ActionOutcome {
    let lines = output.lines().collect::<Vec<_>>();
    if lines.len() != 1 {
        return protocol_error();
    }
    let fields = lines[0].split('|').collect::<Vec<_>>();
    if fields.len() != 4 || fields[1] != expected_name {
        return protocol_error();
    }
    match fields[0] {
        "OK" => ActionOutcome::Pass,
        "FAIL" => ActionOutcome::Fail,
        "WARN" => ActionOutcome::Error(ActionFault::new("doctor_warn", None)),
        "DEFER" => ActionOutcome::Error(ActionFault::new("doctor_defer", None)),
        _ => protocol_error(),
    }
}

fn protocol_error() -> ActionOutcome {
    ActionOutcome::Error(ActionFault::new("doctor_protocol_error", None))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use ostrom_core::{Catalogue, CatalogueEnumeration, CheckDocument, CheckVerdict};
    use tempfile::tempdir;

    use super::{DOCTOR_CHECKS, DoctorProvider, parse_result};
    use crate::{ActionFault, ActionOutcome, ActionRegistry};

    fn catalogue(check: &str) -> CatalogueEnumeration {
        let yaml = format!(
            "checks_version: 1\nchecks:\n  doctor-fixture:\n    uses: doctor/check\n    with:\n      check: {check}\n      timeout: 1s\n"
        );
        CatalogueEnumeration {
            catalogues: vec![Catalogue {
                document: CheckDocument::from_yaml(&yaml).expect("doctor fixture"),
            }],
            complete: true,
        }
    }

    fn fixture_runtime(path: &Path, warn: Option<&str>) {
        let branch = warn.map_or_else(
            || "printf 'OK|%s|fixture detail|' \"$2\"".to_owned(),
            |name| {
                format!(
                    "if [ \"$2\" = {name:?} ]; then printf 'WARN|%s|undetermined|' \"$2\"; else printf 'OK|%s|fixture detail|' \"$2\"; fi"
                )
            },
        );
        fs::write(path, format!("{branch}\n")).expect("write doctor fixture");
    }

    fn registry(script: &Path) -> ActionRegistry {
        let mut registry = ActionRegistry::new();
        registry
            .register(DoctorProvider::with_runtime("sh", script))
            .expect("doctor provider");
        registry
    }

    #[test]
    fn every_doctor_check_is_individually_addressable() {
        let fixture = tempdir().expect("fixture directory");
        let script = fixture.path().join("doctor-fixture.sh");
        fixture_runtime(&script, None);
        let registry = registry(&script);
        for check in DOCTOR_CHECKS {
            let receipt = registry
                .prepare("doctor-fixture", &catalogue(check))
                .unwrap_or_else(|error| panic!("{check} did not prepare: {error}"))
                .execute(&format!("{check}-attempt"));
            assert_eq!(receipt.verdict, Some(CheckVerdict::Pass), "{check}");
        }
    }

    #[test]
    fn warn_is_a_named_error_and_never_a_verdict() {
        let fixture = tempdir().expect("fixture directory");
        let script = fixture.path().join("doctor-fixture.sh");
        fixture_runtime(&script, Some("environment"));
        let receipt = registry(&script)
            .prepare("doctor-fixture", &catalogue("environment"))
            .expect("environment doctor check")
            .execute("warn-attempt");
        assert_eq!(receipt.verdict, None);
        assert_eq!(receipt.error.as_deref(), Some("doctor_warn"));
    }

    #[test]
    fn defer_is_a_named_error_and_never_a_verdict() {
        assert_eq!(
            parse_result("DEFER|provider-reachable|fixture|\n", "provider-reachable"),
            ActionOutcome::Error(ActionFault::new("doctor_defer", None))
        );
    }
}
