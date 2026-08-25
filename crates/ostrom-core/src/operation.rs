use std::collections::BTreeSet;

use serde_yaml::Value;
use thiserror::Error;

use crate::{OperationDecl, OperationParamType, PromptValue, StepDecl};

/// The credential boundary used to execute one operation action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionBoundary {
    /// Ostrom mints a token with the action's exact scopes for its child.
    Mediated,
    /// Ostrom runs the child with ambient forge credentials removed.
    Local,
}

/// Where an action parameter ultimately flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterSink {
    Content,
    Command,
    Data,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionScope {
    pub permission: &'static str,
    pub level: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionParameter {
    pub name: &'static str,
    pub required: bool,
    pub sink: ParameterSink,
    pub caller_supplied: bool,
}

/// Stable metadata for one member of the closed operation action catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationAction {
    pub uses: &'static str,
    pub boundary: ActionBoundary,
    pub scopes: &'static [ActionScope],
    pub ungrantable: bool,
    pub guarded: bool,
    pub parameters: &'static [ActionParameter],
}

const PULL_REQUEST_WRITE: &[ActionScope] = &[ActionScope {
    permission: "pull_requests",
    level: "write",
}];
const MERGE_SCOPES: &[ActionScope] = &[
    ActionScope {
        permission: "contents",
        level: "write",
    },
    ActionScope {
        permission: "pull_requests",
        level: "write",
    },
];
const CONTENTS_WRITE: &[ActionScope] = &[ActionScope {
    permission: "contents",
    level: "write",
}];

const NOTE_PARAMETER: &[ActionParameter] = &[ActionParameter {
    name: "note",
    required: true,
    sink: ParameterSink::Content,
    caller_supplied: true,
}];
const MERGE_PARAMETERS: &[ActionParameter] = &[ActionParameter {
    name: "method",
    required: false,
    sink: ParameterSink::Command,
    caller_supplied: true,
}];
const TAG_PARAMETERS: &[ActionParameter] = &[
    ActionParameter {
        name: "name",
        required: true,
        sink: ParameterSink::Command,
        caller_supplied: true,
    },
    ActionParameter {
        name: "message",
        required: false,
        sink: ParameterSink::Content,
        caller_supplied: true,
    },
];
const COMMAND_PARAMETERS: &[ActionParameter] = &[
    ActionParameter {
        name: "script",
        required: true,
        sink: ParameterSink::Command,
        caller_supplied: false,
    },
    ActionParameter {
        name: "timeout",
        required: false,
        sink: ParameterSink::Data,
        caller_supplied: true,
    },
];
const AGENT_RUN_PARAMETERS: &[ActionParameter] = &[
    ActionParameter {
        name: "prompt",
        required: true,
        sink: ParameterSink::Content,
        caller_supplied: true,
    },
    ActionParameter {
        name: "model",
        required: false,
        sink: ParameterSink::Data,
        caller_supplied: true,
    },
];

const OPERATION_ACTIONS: &[OperationAction] = &[
    OperationAction {
        uses: "agent/claude",
        boundary: ActionBoundary::Local,
        scopes: &[],
        ungrantable: false,
        guarded: false,
        parameters: AGENT_RUN_PARAMETERS,
    },
    OperationAction {
        uses: "gh/post-verdict",
        boundary: ActionBoundary::Mediated,
        scopes: PULL_REQUEST_WRITE,
        ungrantable: false,
        guarded: false,
        parameters: NOTE_PARAMETER,
    },
    OperationAction {
        uses: "gh/merge-pr",
        boundary: ActionBoundary::Mediated,
        scopes: MERGE_SCOPES,
        ungrantable: false,
        guarded: true,
        parameters: MERGE_PARAMETERS,
    },
    OperationAction {
        uses: "git/tag",
        boundary: ActionBoundary::Mediated,
        scopes: CONTENTS_WRITE,
        ungrantable: false,
        guarded: true,
        parameters: TAG_PARAMETERS,
    },
    OperationAction {
        uses: "cmd/run",
        boundary: ActionBoundary::Local,
        scopes: &[],
        ungrantable: false,
        guarded: false,
        parameters: COMMAND_PARAMETERS,
    },
    // Kept in the catalogue so policy can name the system capability and get
    // the security-relevant error. It has no dispatcher implementation.
    OperationAction {
        uses: "sys/enable-loop",
        boundary: ActionBoundary::Local,
        scopes: &[],
        ungrantable: true,
        guarded: false,
        parameters: &[],
    },
];

#[must_use]
pub fn operation_actions() -> &'static [OperationAction] {
    OPERATION_ACTIONS
}

#[must_use]
pub fn operation_action(uses: &str) -> Option<&'static OperationAction> {
    OPERATION_ACTIONS.iter().find(|action| action.uses == uses)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRunParameters {
    pub prompt: PromptValue,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("invalid_agent_run_parameters")]
pub struct AgentRunParametersError;

/// Decode the core-owned operation-side `agent/*` parameters. Unlike a judged
/// check, an operation agent receives no evidence or freshness references.
pub fn agent_run_parameters(
    step: &StepDecl,
) -> Result<AgentRunParameters, AgentRunParametersError> {
    if step.uses.split_once('/').map(|part| part.0) != Some("agent")
        || step
            .parameters
            .keys()
            .any(|key| !["model", "prompt"].contains(&key.as_str()))
    {
        return Err(AgentRunParametersError);
    }
    let prompt = step
        .parameters
        .get("prompt")
        .cloned()
        .ok_or(AgentRunParametersError)
        .and_then(|value| serde_yaml::from_value(value).map_err(|_| AgentRunParametersError))?;
    let model = step
        .parameters
        .get("model")
        .map(|value| {
            value
                .as_str()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .ok_or(AgentRunParametersError)
        })
        .transpose()?;
    Ok(AgentRunParameters { prompt, model })
}

pub fn validate_operation(
    name: &str,
    operation: &OperationDecl,
) -> Result<(), OperationActionError> {
    for (param, declaration) in &operation.params {
        if param.is_empty()
            || !param
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(OperationActionError::InvalidParam {
                operation: name.to_owned(),
                param: param.clone(),
                message: "name must contain only lowercase letters, digits, or `_`".to_owned(),
            });
        }
        declaration
            .validate()
            .map_err(|message| OperationActionError::InvalidParam {
                operation: name.to_owned(),
                param: param.clone(),
                message,
            })?;
    }

    for (index, step) in operation.steps.iter().enumerate() {
        let action =
            operation_action(&step.uses).ok_or_else(|| OperationActionError::UnknownAction {
                operation: name.to_owned(),
                step: index,
                uses: step.uses.clone(),
            })?;
        if action.ungrantable {
            return Err(OperationActionError::UngrantableAction {
                operation: name.to_owned(),
                step: index,
                uses: step.uses.clone(),
            });
        }
        if action.guarded && step.requires.is_empty() {
            return Err(OperationActionError::MissingGuard {
                operation: name.to_owned(),
                step: index,
                uses: step.uses.clone(),
            });
        }
        validate_parameters(name, index, operation, action, &step.parameters)?;
        if action.uses == "agent/claude" {
            agent_run_parameters(step).map_err(|_| {
                OperationActionError::InvalidAgentRunParameters {
                    operation: name.to_owned(),
                    step: index,
                }
            })?;
        }
    }
    Ok(())
}

fn validate_parameters(
    operation_name: &str,
    step: usize,
    operation: &OperationDecl,
    action: &OperationAction,
    parameters: &std::collections::BTreeMap<String, Value>,
) -> Result<(), OperationActionError> {
    for key in parameters.keys() {
        if !action
            .parameters
            .iter()
            .any(|parameter| parameter.name == key)
        {
            return Err(OperationActionError::UnknownActionParameter {
                operation: operation_name.to_owned(),
                step,
                uses: action.uses.to_owned(),
                parameter: key.clone(),
            });
        }
    }
    for parameter in action.parameters {
        let value = parameters.get(parameter.name);
        if parameter.required && value.is_none() {
            return Err(OperationActionError::MissingActionParameter {
                operation: operation_name.to_owned(),
                step,
                uses: action.uses.to_owned(),
                parameter: parameter.name.to_owned(),
            });
        }
        let Some(value) = value else {
            continue;
        };
        let references = param_references(value)?;
        for reference in references {
            let declaration = operation.params.get(&reference).ok_or_else(|| {
                OperationActionError::UnknownParamReference {
                    operation: operation_name.to_owned(),
                    step,
                    param: reference.clone(),
                }
            })?;
            if !parameter.caller_supplied {
                return Err(OperationActionError::CallerSuppliedActionParameter {
                    operation: operation_name.to_owned(),
                    step,
                    uses: action.uses.to_owned(),
                    parameter: parameter.name.to_owned(),
                });
            }
            if declaration.kind == OperationParamType::Markdown
                && parameter.sink == ParameterSink::Command
            {
                return Err(OperationActionError::MarkdownCommandFlow {
                    operation: operation_name.to_owned(),
                    step,
                    param: reference,
                    uses: action.uses.to_owned(),
                    parameter: parameter.name.to_owned(),
                });
            }
        }
    }
    Ok(())
}

fn param_references(value: &Value) -> Result<BTreeSet<String>, OperationActionError> {
    let mut references = BTreeSet::new();
    collect_param_references(value, &mut references)?;
    Ok(references)
}

fn collect_param_references(
    value: &Value,
    references: &mut BTreeSet<String>,
) -> Result<(), OperationActionError> {
    match value {
        Value::String(value) => {
            if let Some(name) = value.strip_prefix("$params.") {
                if name.is_empty() || name.contains(['$', '{', '}', ' ', '/', '.']) {
                    return Err(OperationActionError::InvalidParamReference(value.clone()));
                }
                references.insert(name.to_owned());
            } else if value.contains("$params.") {
                return Err(OperationActionError::InvalidParamReference(value.clone()));
            }
        }
        Value::Sequence(values) => {
            for value in values {
                collect_param_references(value, references)?;
            }
        }
        Value::Mapping(values) => {
            for (key, value) in values {
                collect_param_references(key, references)?;
                collect_param_references(value, references)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Tagged(_) => {}
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OperationActionError {
    #[error("operation `{operation}` param `{param}` is invalid: {message}")]
    InvalidParam {
        operation: String,
        param: String,
        message: String,
    },
    #[error("operation `{operation}` step {step} names unknown action `{uses}`")]
    UnknownAction {
        operation: String,
        step: usize,
        uses: String,
    },
    #[error("operation `{operation}` step {step} names ungrantable action `{uses}`")]
    UngrantableAction {
        operation: String,
        step: usize,
        uses: String,
    },
    #[error("operation `{operation}` step {step} guarded action `{uses}` has no `requires`")]
    MissingGuard {
        operation: String,
        step: usize,
        uses: String,
    },
    #[error(
        "operation `{operation}` step {step} action `{uses}` has unknown parameter `{parameter}`"
    )]
    UnknownActionParameter {
        operation: String,
        step: usize,
        uses: String,
        parameter: String,
    },
    #[error("operation `{operation}` step {step} action `{uses}` requires parameter `{parameter}`")]
    MissingActionParameter {
        operation: String,
        step: usize,
        uses: String,
        parameter: String,
    },
    #[error(
        "invalid operation param reference `{0}`; references must be whole `$params.<name>` values"
    )]
    InvalidParamReference(String),
    #[error("operation `{operation}` step {step} references unknown param `{param}`")]
    UnknownParamReference {
        operation: String,
        step: usize,
        param: String,
    },
    #[error(
        "operation `{operation}` step {step} action `{uses}` parameter `{parameter}` may not be caller supplied"
    )]
    CallerSuppliedActionParameter {
        operation: String,
        step: usize,
        uses: String,
        parameter: String,
    },
    #[error(
        "operation `{operation}` step {step} markdown param `{param}` may not reach command sink `{uses}.{parameter}`"
    )]
    MarkdownCommandFlow {
        operation: String,
        step: usize,
        param: String,
        uses: String,
        parameter: String,
    },
    #[error("operation `{operation}` step {step} has invalid `agent/claude` parameters")]
    InvalidAgentRunParameters { operation: String, step: usize },
}

#[cfg(test)]
mod tests {
    use crate::{
        ActionBoundary, ParameterSink, PolicyManifest, agent_run_parameters, operation_action,
    };

    fn agent_step(parameters: &str) -> crate::StepDecl {
        serde_yaml::from_str(&format!("uses: agent/claude\nwith:\n{parameters}"))
            .expect("agent step fixture")
    }

    #[test]
    fn ungrantable_action_fails_at_every_position() {
        for steps in [
            "      - uses: sys/enable-loop\n      - uses: cmd/run\n        with: {script: 'true'}\n",
            "      - uses: cmd/run\n        with: {script: 'true'}\n      - uses: sys/enable-loop\n",
        ] {
            let yaml = format!("manifest_version: 1\noperations:\n  wrapped:\n    steps:\n{steps}");
            let error = PolicyManifest::from_yaml(&yaml).expect_err("ungrantable action fails");
            assert!(
                error
                    .to_string()
                    .contains("ungrantable action `sys/enable-loop`")
            );
        }
    }

    #[test]
    fn markdown_may_reach_content_but_not_a_command_sink() {
        let content = "manifest_version: 1\noperations:\n  comment:\n    params:\n      note: {type: markdown}\n    steps:\n      - uses: gh/post-verdict\n        with: {note: '$params.note'}\n";
        PolicyManifest::from_yaml(content).expect("markdown content flow is valid");

        let command = content.replace("gh/post-verdict", "gh/merge-pr").replace(
            "with: {note: '$params.note'}",
            "with: {method: '$params.note'}\n        requires: ready",
        );
        let error = PolicyManifest::from_yaml(&command).expect_err("command flow fails");
        assert!(error.to_string().contains("may not reach command sink"));
    }

    #[test]
    fn cmd_script_cannot_reference_any_caller_param() {
        let yaml = "manifest_version: 1\noperations:\n  command:\n    params:\n      version: {type: semver}\n    steps:\n      - uses: cmd/run\n        with: {script: '$params.version'}\n";
        let error = PolicyManifest::from_yaml(yaml).expect_err("caller script fails");
        assert!(error.to_string().contains("may not be caller supplied"));
    }

    #[test]
    fn operation_param_type_catalogue_is_closed() {
        for kind in ["string", "integer", "shell", "anything"] {
            let yaml = format!(
                "manifest_version: 1\noperations:\n  typed:\n    params:\n      value: {{type: {kind}}}\n    steps: []\n"
            );
            let error = PolicyManifest::from_yaml(&yaml).expect_err("unknown type fails");
            assert!(error.to_string().contains("unknown variant"), "{error}");
        }
    }

    #[test]
    fn enum_and_semver_restrict_command_values() {
        let valid = "manifest_version: 1\noperations:\n  release:\n    params:\n      method: {type: enum, values: [merge, squash], default: squash}\n      version: {type: semver, default: 1.2.3}\n    steps:\n      - uses: git/tag\n        with: {name: '$params.version'}\n        requires: ready\n";
        PolicyManifest::from_yaml(valid).expect("safe command types parse");

        let bad_enum = valid.replace("default: squash", "default: force");
        assert!(
            PolicyManifest::from_yaml(&bad_enum)
                .expect_err("enum default fails")
                .to_string()
                .contains("outside the declared enum")
        );
        let bad_semver = valid.replace("default: 1.2.3", "default: '1.2.3; command'");
        assert!(
            PolicyManifest::from_yaml(&bad_semver)
                .expect_err("unsafe semver fails")
                .to_string()
                .contains("expected semantic version")
        );
    }

    #[test]
    fn catalogue_declares_boundary_scopes_and_guards() {
        let agent = operation_action("agent/claude").expect("agent action");
        assert_eq!(agent.boundary, ActionBoundary::Local);
        assert!(agent.scopes.is_empty());
        assert!(!agent.ungrantable);
        assert!(!agent.guarded);
        assert_eq!(
            agent
                .parameters
                .iter()
                .map(|parameter| {
                    (
                        parameter.name,
                        parameter.required,
                        parameter.sink,
                        parameter.caller_supplied,
                    )
                })
                .collect::<Vec<_>>(),
            [
                ("prompt", true, ParameterSink::Content, true),
                ("model", false, ParameterSink::Data, true),
            ]
        );
        let merge = operation_action("gh/merge-pr").expect("merge action");
        assert_eq!(merge.boundary, ActionBoundary::Mediated);
        assert!(merge.guarded);
        assert_eq!(
            merge
                .scopes
                .iter()
                .map(|scope| (scope.permission, scope.level))
                .collect::<Vec<_>>(),
            [("contents", "write"), ("pull_requests", "write")]
        );
        let command = operation_action("cmd/run").expect("command action");
        assert_eq!(command.boundary, ActionBoundary::Local);
        assert!(command.scopes.is_empty());
    }

    #[test]
    fn agent_run_parameters_are_bounded_to_prompt_and_model() {
        let parameters = agent_run_parameters(&agent_step(
            "  prompt: inspect the repository\n  model: opus\n",
        ))
        .expect("bounded agent parameters");
        assert_eq!(
            parameters.prompt,
            crate::PromptValue::Inline("inspect the repository".to_owned())
        );
        assert_eq!(parameters.model.as_deref(), Some("opus"));

        for parameters in [
            "  model: opus\n",
            "  prompt: ''\n",
            "  prompt: 42\n",
            "  prompt: {from: ''}\n",
            "  prompt: {from: prompt.md, extra: nope}\n",
            "  prompt: prompts.\n",
            "  prompt: inspect\n  model: ''\n",
            "  prompt: inspect\n  model: 42\n",
            "  prompt: inspect\n  evidence: []\n",
            "  prompt: inspect\n  fresh_for: 1h\n",
            "  prompt: inspect\n  provider_payload: nope\n",
        ] {
            assert!(
                agent_run_parameters(&agent_step(parameters)).is_err(),
                "accepted {parameters}"
            );
        }

        let non_agent = serde_yaml::from_str("uses: cmd/run\nwith: {prompt: inspect}\n")
            .expect("non-agent step fixture");
        assert!(agent_run_parameters(&non_agent).is_err());
    }

    #[test]
    fn agent_run_parameters_accept_file_and_named_prompts() {
        let file = agent_run_parameters(&agent_step("  prompt: {from: ./prompt.md}\n"))
            .expect("file prompt");
        assert_eq!(file.prompt.file(), Some("./prompt.md"));

        let named = agent_run_parameters(&agent_step("  prompt: prompts.builder-work\n"))
            .expect("named prompt");
        assert_eq!(named.prompt.named(), Some("builder-work"));
    }

    #[test]
    fn agent_operation_accepts_its_bounded_shape_and_refuses_unknown_keys() {
        let valid = "manifest_version: 1\noperations:\n  inspect:\n    steps:\n      - uses: agent/claude\n        with: {prompt: inspect, model: opus}\n";
        PolicyManifest::from_yaml(valid).expect("agent operation is valid");

        for key in ["evidence", "fresh_for", "provider_payload"] {
            let invalid = valid.replace(
                "with: {prompt: inspect, model: opus}",
                &format!("with: {{prompt: inspect, model: opus, {key}: nope}}"),
            );
            let error = PolicyManifest::from_yaml(&invalid).expect_err("unknown key fails");
            assert!(
                error
                    .to_string()
                    .contains(&format!("unknown parameter `{key}`")),
                "{error}"
            );
        }
    }

    #[test]
    fn agent_operation_refuses_unknown_named_prompt() {
        let error = PolicyManifest::from_yaml(
            "manifest_version: 1\noperations:\n  inspect:\n    steps:\n      - uses: agent/claude\n        with: {prompt: prompts.missing}\n",
        )
        .expect_err("unknown named prompt");
        assert!(
            error
                .to_string()
                .contains("unknown prompt `prompts.missing`")
        );
    }

    #[test]
    fn manifest_resolver_materializes_inline_and_named_prompt_text() {
        let manifest = PolicyManifest::from_yaml(
            "manifest_version: 1\nprompts: {shared: 'named text'}\noperations:\n  inline:\n    steps:\n      - uses: agent/claude\n        with: {prompt: 'inline text'}\n  named:\n    steps:\n      - uses: agent/claude\n        with: {prompt: prompts.shared}\n",
        )
        .expect("prompt manifest");
        assert_eq!(
            manifest
                .resolve_operation_prompt("inline")
                .expect("inline prompt"),
            "inline text"
        );
        assert_eq!(
            manifest
                .resolve_operation_prompt("named")
                .expect("named prompt"),
            "named text"
        );
        let round_trip = manifest.to_yaml().expect("render prompt manifest");
        assert!(
            round_trip.contains("prompt: prompts.shared"),
            "{round_trip}"
        );
    }
}
