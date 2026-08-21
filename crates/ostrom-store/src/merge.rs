use std::process::Output;

use serde_json::Value;
use thiserror::Error;

use crate::{OstromPaths, app_token::CredentialCommandError, credential_output};

use super::gate::latest_verdict;

pub const MERGE_REFUSED_EXIT_CODE: i32 = 3;
pub const MERGE_ATTEMPT_FAILED_EXIT_CODE: i32 = 4;

#[derive(Debug, Clone)]
pub struct MergeOptions {
    pub paths: OstromPaths,
    pub repository: String,
    pub pr_number: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Debug, Error)]
pub enum MergeError {
    #[error(
        "usage: ostrom merge <owner/repo> <pr-number>\nexit codes: 3 = merge refused by the recorded verdict; 4 = merge attempted at GitHub and failed"
    )]
    InvalidArguments,
    #[error("ostrom merge: {operation} credential failed: {source}")]
    Credential {
        operation: &'static str,
        #[source]
        source: CredentialCommandError,
    },
}

impl MergeError {
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::InvalidArguments => 64,
            Self::Credential { source, .. } => source.exit_code(),
        }
    }
}

pub fn run_merge(options: &MergeOptions) -> Result<MergeOutput, MergeError> {
    let number = parse_arguments(&options.repository, &options.pr_number)?;
    let head = credential_output(
        &options.paths,
        "gatekeeper",
        &options.repository,
        &options.repository,
        "metadata:read,pull_requests:read",
        &[
            "gh",
            "pr",
            "view",
            number,
            "--repo",
            &options.repository,
            "--json",
            "headRefOid",
        ],
    )
    .map_err(|source| MergeError::Credential {
        operation: "head lookup",
        source,
    })?;
    if !head.status.success() {
        return Ok(command_failure(
            head,
            format!(
                "ostrom merge: could not resolve the current head SHA for {}#{}",
                options.repository, number
            ),
            2,
        ));
    }
    let Some(head_sha) = serde_json::from_slice::<Value>(&head.stdout)
        .ok()
        .and_then(|value| value["headRefOid"].as_str().map(str::to_owned))
        .filter(|value| !value.is_empty())
    else {
        return Ok(MergeOutput {
            stdout: String::new(),
            stderr: format!(
                "ostrom merge: could not resolve the current head SHA for {}#{}: GitHub returned no headRefOid\n",
                options.repository, number
            ),
            exit_code: 2,
        });
    };

    let target = format!("{}#{number}", options.repository);
    let verdict = latest_verdict(&options.paths.state.join("gate.jsonl"), &target, &head_sha);
    if verdict.as_deref() != Some("pass") {
        let found = verdict.unwrap_or_else(|| "none (no verdict recorded)".to_owned());
        return Ok(MergeOutput {
            stdout: String::new(),
            stderr: format!(
                "ostrom merge: refused {target} at head_sha={head_sha}: verdict={found}\n"
            ),
            exit_code: MERGE_REFUSED_EXIT_CODE,
        });
    }

    let merge = credential_output(
        &options.paths,
        "gatekeeper",
        &options.repository,
        &options.repository,
        "metadata:read,contents:write,pull_requests:write",
        &["gh", "pr", "merge", number, "--repo", &options.repository],
    )
    .map_err(|source| MergeError::Credential {
        operation: "merge",
        source,
    })?;
    if merge.status.success() {
        return Ok(MergeOutput {
            stdout: String::from_utf8_lossy(&merge.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&merge.stderr).into_owned(),
            exit_code: 0,
        });
    }
    Ok(command_failure(
        merge,
        format!("ostrom merge: GitHub merge attempt failed for {target} at head_sha={head_sha}"),
        MERGE_ATTEMPT_FAILED_EXIT_CODE,
    ))
}

fn parse_arguments<'a>(repository: &'a str, number: &'a str) -> Result<&'a str, MergeError> {
    if repository.chars().any(char::is_whitespace)
        || number.chars().any(char::is_whitespace)
        || number.starts_with('0')
        || !number.bytes().all(|byte| byte.is_ascii_digit())
        || number
            .parse::<u64>()
            .ok()
            .filter(|value| *value != 0)
            .is_none()
    {
        return Err(MergeError::InvalidArguments);
    }
    let mut parts = repository.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(MergeError::InvalidArguments);
    };
    if owner.is_empty() || name.is_empty() || owner.contains('#') || name.contains('#') {
        return Err(MergeError::InvalidArguments);
    }
    Ok(number)
}

fn command_failure(output: Output, message: String, exit_code: i32) -> MergeOutput {
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !stderr.is_empty() && !stderr.ends_with('\n') {
        stderr.push('\n');
    }
    stderr.push_str(&message);
    stderr.push('\n');
    MergeOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr,
        exit_code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_arguments_are_exact() {
        assert_eq!(parse_arguments("placeholder-org/alpha", "7").unwrap(), "7");
        for (repository, number) in [
            ("placeholder-org/alpha/extra", "7"),
            ("placeholder-org", "7"),
            ("placeholder-org/alpha", "0"),
            ("placeholder-org/alpha", "07"),
            ("placeholder-org/alpha", "not-a-number"),
        ] {
            assert!(parse_arguments(repository, number).is_err());
        }
    }
}
