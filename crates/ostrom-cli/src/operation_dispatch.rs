use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use ostrom_core::{
    OperationAction, OperationDecl, OperationParamType, PolicyCandidate, PolicyManifest,
};
use serde_yaml::Value;
use thiserror::Error;

#[derive(Debug)]
pub(crate) struct OperationInvocation {
    pub(crate) name: String,
    pub(crate) target: String,
    pub(crate) parameters: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedOperationTarget {
    pub(crate) raw: String,
    pub(crate) repository: String,
    pub(crate) candidate: PolicyCandidate,
}

pub(crate) trait OperationRuntime {
    fn resolve_target(
        &mut self,
        raw: &str,
        actor: &str,
        operation: &str,
    ) -> Result<ResolvedOperationTarget, OperationDispatchError>;

    fn require(
        &mut self,
        check: &str,
        target: &ResolvedOperationTarget,
    ) -> Result<(), OperationDispatchError>;

    fn execute(
        &mut self,
        action: &'static OperationAction,
        target: &ResolvedOperationTarget,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<(), OperationDispatchError>;
}

pub(crate) fn parse_invocation(
    manifest: &PolicyManifest,
    arguments: &[OsString],
) -> Result<OperationInvocation, OperationDispatchError> {
    let strings = arguments
        .iter()
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .ok_or(OperationDispatchError::NonUnicodeArgument)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let Some(name) = strings.first() else {
        return Err(OperationDispatchError::MissingOperation);
    };
    let operation = manifest
        .operations
        .get(name)
        .ok_or_else(|| OperationDispatchError::UnknownOperation(name.clone()))?;
    let Some(target) = strings.get(1).filter(|target| !target.starts_with('-')) else {
        return Err(OperationDispatchError::MissingTarget(name.clone()));
    };
    let parameters = parse_flags(operation, &strings[2..])?;
    Ok(OperationInvocation {
        name: name.clone(),
        target: target.clone(),
        parameters,
    })
}

fn parse_flags(
    operation: &OperationDecl,
    arguments: &[String],
) -> Result<BTreeMap<String, Value>, OperationDispatchError> {
    let mut supplied = BTreeMap::new();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        let Some(flag) = argument.strip_prefix("--") else {
            return Err(OperationDispatchError::UnexpectedArgument(argument.clone()));
        };
        let (wire_name, raw, consumed) = if let Some((name, value)) = flag.split_once('=') {
            (name, value.to_owned(), 1)
        } else {
            let value = arguments
                .get(index + 1)
                .filter(|value| !value.starts_with("--"))
                .ok_or_else(|| OperationDispatchError::MissingFlagValue(flag.to_owned()))?;
            (flag, value.clone(), 2)
        };
        let name = wire_name.replace('-', "_");
        let declaration = operation
            .params
            .get(&name)
            .ok_or_else(|| OperationDispatchError::UnknownFlag(wire_name.to_owned()))?;
        if supplied.contains_key(&name) {
            return Err(OperationDispatchError::DuplicateFlag(wire_name.to_owned()));
        }
        let raw = if declaration.kind == OperationParamType::Markdown {
            read_markdown_argument(&raw)?
        } else {
            raw
        };
        let value = Value::String(raw);
        declaration.validate_value(&value).map_err(|message| {
            OperationDispatchError::InvalidFlag {
                flag: wire_name.to_owned(),
                message,
            }
        })?;
        supplied.insert(name, value);
        index += consumed;
    }

    for (name, declaration) in &operation.params {
        if !supplied.contains_key(name) {
            if let Some(default) = &declaration.default {
                supplied.insert(name.clone(), default.clone());
            } else {
                return Err(OperationDispatchError::MissingFlag(name.replace('_', "-")));
            }
        }
    }
    Ok(supplied)
}

fn read_markdown_argument(raw: &str) -> Result<String, OperationDispatchError> {
    let Some(path) = raw.strip_prefix('@') else {
        return Ok(raw.to_owned());
    };
    if path.is_empty() {
        return Err(OperationDispatchError::UnreadableMarkdown(PathBuf::from(
            path,
        )));
    }
    fs::read_to_string(path)
        .map_err(|_| OperationDispatchError::UnreadableMarkdown(PathBuf::from(path)))
}

pub(crate) fn dispatch_operation(
    manifest: &PolicyManifest,
    actor: &str,
    invocation: &OperationInvocation,
    runtime: &mut impl OperationRuntime,
) -> Result<(), OperationDispatchError> {
    if !manifest.actors.contains_key(actor) {
        return Err(OperationDispatchError::UnknownActor(actor.to_owned()));
    }
    let operation = manifest
        .operations
        .get(&invocation.name)
        .ok_or_else(|| OperationDispatchError::UnknownOperation(invocation.name.clone()))?;

    // Target resolution is deliberately before the authorization decision.
    // A later step receives this immutable value and has no target parameter
    // through which it could widen the grant.
    let target = runtime.resolve_target(&invocation.target, actor, &invocation.name)?;
    let decision = manifest.decide(actor, &invocation.name, &target.candidate);
    if !decision.granted {
        return Err(OperationDispatchError::NotAuthorized {
            actor: actor.to_owned(),
            operation: invocation.name.clone(),
            target: invocation.target.clone(),
        });
    }

    for step in &operation.steps {
        let action = ostrom_core::operation_action(&step.uses)
            .ok_or_else(|| OperationDispatchError::UnknownAction(step.uses.clone()))?;
        let parameters = resolve_parameters(&step.parameters, &invocation.parameters)?;
        if let Some(check) = &step.requires {
            runtime.require(check, &target)?;
        }
        runtime.execute(action, &target, &parameters)?;
    }
    Ok(())
}

fn resolve_parameters(
    parameters: &BTreeMap<String, Value>,
    supplied: &BTreeMap<String, Value>,
) -> Result<BTreeMap<String, Value>, OperationDispatchError> {
    parameters
        .iter()
        .map(|(name, value)| {
            resolve_value(value, supplied).map(|resolved| (name.clone(), resolved))
        })
        .collect()
}

fn resolve_value(
    value: &Value,
    supplied: &BTreeMap<String, Value>,
) -> Result<Value, OperationDispatchError> {
    match value {
        Value::String(value) => value.strip_prefix("$params.").map_or_else(
            || Ok(Value::String(value.clone())),
            |name| {
                supplied
                    .get(name)
                    .cloned()
                    .ok_or_else(|| OperationDispatchError::MissingResolvedParam(name.to_owned()))
            },
        ),
        Value::Sequence(values) => values
            .iter()
            .map(|value| resolve_value(value, supplied))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Sequence),
        Value::Mapping(values) => values
            .iter()
            .map(|(key, value)| {
                Ok((
                    resolve_value(key, supplied)?,
                    resolve_value(value, supplied)?,
                ))
            })
            .collect::<Result<serde_yaml::Mapping, _>>()
            .map(Value::Mapping),
        Value::Tagged(value) => resolve_value(&value.value, supplied),
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(value.clone()),
    }
}

pub(crate) fn resolve_repository_target(
    raw: &str,
    actor: &str,
    operation: &str,
) -> Result<ResolvedOperationTarget, OperationDispatchError> {
    if raw.starts_with('-') || raw.chars().any(char::is_whitespace) {
        return Err(OperationDispatchError::InvalidTarget(raw.to_owned()));
    }
    let repository = raw
        .split_once('#')
        .map_or(raw, |(repository, _)| repository);
    let mut components = repository.split('/');
    let valid = matches!(
        (components.next(), components.next(), components.next()),
        (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty()
    );
    if !valid {
        return Err(OperationDispatchError::InvalidTarget(raw.to_owned()));
    }
    if let Some((_, number)) = raw.split_once('#')
        && (number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(OperationDispatchError::InvalidTarget(raw.to_owned()));
    }
    Ok(ResolvedOperationTarget {
        raw: raw.to_owned(),
        repository: repository.to_owned(),
        candidate: PolicyCandidate {
            repository: repository.to_owned(),
            actor: Some(actor.to_owned()),
            verb: Some(operation.to_owned()),
            ..PolicyCandidate::default()
        },
    })
}

pub(crate) fn manifest_path(config: &Path) -> PathBuf {
    std::env::var_os("OSTROM_POLICY_MANIFEST")
        .filter(|path| !path.is_empty())
        .map_or_else(|| config.join("policy.yaml"), PathBuf::from)
}

#[derive(Debug, Error)]
pub(crate) enum OperationDispatchError {
    #[error("operation name is missing")]
    MissingOperation,
    #[error("operation arguments must be valid Unicode")]
    NonUnicodeArgument,
    #[error("unknown operation `{0}`")]
    UnknownOperation(String),
    #[error("operation `{0}` requires a positional target")]
    MissingTarget(String),
    #[error("unexpected operation argument `{0}`")]
    UnexpectedArgument(String),
    #[error("flag `--{0}` requires a value")]
    MissingFlagValue(String),
    #[error("unknown operation flag `--{0}`")]
    UnknownFlag(String),
    #[error("operation flag `--{0}` was supplied more than once")]
    DuplicateFlag(String),
    #[error("required operation flag `--{0}` is missing")]
    MissingFlag(String),
    #[error("operation flag `--{flag}` is invalid: {message}")]
    InvalidFlag { flag: String, message: String },
    #[error("could not read markdown argument from `{0}`")]
    UnreadableMarkdown(PathBuf),
    #[error("unknown operation actor `{0}`")]
    UnknownActor(String),
    #[error("invalid operation target `{0}`; expected owner/repository or owner/repository#number")]
    InvalidTarget(String),
    #[error("could not resolve operation target `{target}`: {message}")]
    TargetResolutionFailed { target: String, message: String },
    #[error("actor `{actor}` is not authorized for operation `{operation}` on `{target}`")]
    NotAuthorized {
        actor: String,
        operation: String,
        target: String,
    },
    #[error("unknown operation action `{0}`")]
    UnknownAction(String),
    #[error("operation parameter `{0}` was not resolved")]
    MissingResolvedParam(String),
    #[error("required check `{0}` did not pass")]
    RequirementFailed(String),
    #[error("operation action `{action}` failed: {message}")]
    ActionFailed { action: String, message: String },
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString};

    use ostrom_core::{OperationAction, PolicyManifest};
    use serde_yaml::Value;

    use super::{
        OperationDispatchError, OperationRuntime, ResolvedOperationTarget, dispatch_operation,
        parse_invocation, resolve_repository_target,
    };

    const MANIFEST: &str = "manifest_version: 1\nactors: {builder: {}}\noperations:\n  deliver:\n    params:\n      version: {type: semver}\n    steps:\n      - uses: cmd/run\n        with: {script: 'first'}\n      - uses: git/tag\n        with: {name: '$params.version'}\n        requires: ready\ngrants:\n  builder-deliver: {actors: builder, operations: deliver, repositories: placeholder-org/repo}\n";

    #[derive(Default)]
    struct RecordingRuntime {
        events: Vec<String>,
    }

    impl OperationRuntime for RecordingRuntime {
        fn resolve_target(
            &mut self,
            raw: &str,
            actor: &str,
            operation: &str,
        ) -> Result<ResolvedOperationTarget, OperationDispatchError> {
            self.events.push(format!("resolve:{raw}"));
            resolve_repository_target(raw, actor, operation)
        }

        fn require(
            &mut self,
            check: &str,
            _target: &ResolvedOperationTarget,
        ) -> Result<(), OperationDispatchError> {
            self.events.push(format!("require:{check}"));
            Ok(())
        }

        fn execute(
            &mut self,
            action: &'static OperationAction,
            _target: &ResolvedOperationTarget,
            parameters: &BTreeMap<String, Value>,
        ) -> Result<(), OperationDispatchError> {
            self.events.push(format!(
                "execute:{}:{}",
                action.uses,
                parameters
                    .values()
                    .next()
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ));
            Ok(())
        }
    }

    #[test]
    fn one_grant_dispatches_every_step_in_order_with_the_guard_between_them() {
        let manifest = PolicyManifest::from_yaml(MANIFEST).expect("manifest");
        let arguments = [
            OsString::from("deliver"),
            OsString::from("placeholder-org/repo#7"),
            OsString::from("--version"),
            OsString::from("1.2.3"),
        ];
        let invocation = parse_invocation(&manifest, &arguments).expect("invocation");
        let mut runtime = RecordingRuntime::default();
        dispatch_operation(&manifest, "builder", &invocation, &mut runtime).expect("dispatch");
        assert_eq!(
            runtime.events,
            [
                "resolve:placeholder-org/repo#7",
                "execute:cmd/run:first",
                "require:ready",
                "execute:git/tag:1.2.3",
            ]
        );
    }

    #[test]
    fn an_action_identifier_is_not_an_invocable_operation() {
        let manifest = PolicyManifest::from_yaml(MANIFEST).expect("manifest");
        let arguments = [
            OsString::from("git/tag"),
            OsString::from("placeholder-org/repo"),
        ];
        let error = parse_invocation(&manifest, &arguments).expect_err("action is not operation");
        assert!(matches!(error, OperationDispatchError::UnknownOperation(_)));
    }

    #[test]
    fn target_is_resolved_before_a_denied_operation_stops() {
        let manifest = PolicyManifest::from_yaml(MANIFEST).expect("manifest");
        let arguments = [
            OsString::from("deliver"),
            OsString::from("other-org/repo"),
            OsString::from("--version=1.2.3"),
        ];
        let invocation = parse_invocation(&manifest, &arguments).expect("invocation");
        let mut runtime = RecordingRuntime::default();
        let error = dispatch_operation(&manifest, "builder", &invocation, &mut runtime)
            .expect_err("repository grant denies");
        assert!(matches!(
            error,
            OperationDispatchError::NotAuthorized { .. }
        ));
        assert_eq!(runtime.events, ["resolve:other-org/repo"]);
    }
}
