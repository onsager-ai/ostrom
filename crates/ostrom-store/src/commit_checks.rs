use std::fmt;

use serde_json::{Value, json};

const PAGE_SIZE: usize = 100;
const PAGE_LIMIT: usize = 100;

#[derive(Debug, Clone)]
pub struct CommitChecks {
    pub checks: Vec<Value>,
    pub statuses_error: Option<CheckReadError>,
}

#[derive(Debug, Clone, Copy)]
enum CheckSource {
    CheckRuns,
    Statuses,
}

impl CheckSource {
    const fn name(self) -> &'static str {
        match self {
            Self::CheckRuns => "check-runs",
            Self::Statuses => "commit status",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CheckReadError {
    source: CheckSource,
    detail: String,
}

impl CheckReadError {
    fn new(source: CheckSource, detail: impl Into<String>) -> Self {
        Self {
            source,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for CheckReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.source.name(), self.detail)
    }
}

/// Read every check run and commit status for one commit and normalize the two
/// REST representations into the former GraphQL rollup shape.
///
/// Check runs map `status` to uppercase `state` and uppercase a non-null
/// `conclusion`; an incomplete run retains `conclusion: null`. Commit statuses
/// map `context` to `name` and uppercase `state`. This keeps all condition
/// evaluators on one flat `{name, conclusion, state, __typename}` list.
pub fn read_commit_checks<F>(
    repository: &str,
    sha: &str,
    mut fetch: F,
) -> Result<CommitChecks, CheckReadError>
where
    F: FnMut(&str) -> Result<Vec<u8>, String>,
{
    let checks = read_pages(
        repository,
        sha,
        CheckSource::CheckRuns,
        "check_runs",
        normalize_check_run,
        &mut fetch,
    )?;
    let statuses = read_pages(
        repository,
        sha,
        CheckSource::Statuses,
        "statuses",
        normalize_status,
        &mut fetch,
    );
    match statuses {
        Ok(statuses) => Ok(CommitChecks {
            checks: checks.into_iter().chain(statuses).collect(),
            statuses_error: None,
        }),
        Err(error) => Ok(CommitChecks {
            checks,
            statuses_error: Some(error),
        }),
    }
}

fn read_pages<F, N>(
    repository: &str,
    sha: &str,
    source: CheckSource,
    array_field: &str,
    normalize: N,
    fetch: &mut F,
) -> Result<Vec<Value>, CheckReadError>
where
    F: FnMut(&str) -> Result<Vec<u8>, String>,
    N: Fn(&Value) -> Result<Value, String>,
{
    let mut expected_total = None;
    let mut normalized = Vec::new();
    for page in 1..=PAGE_LIMIT {
        let endpoint = endpoint(repository, sha, source, page);
        let body = fetch(&endpoint).map_err(|error| CheckReadError::new(source, error))?;
        let response = serde_json::from_slice::<Value>(&body).map_err(|error| {
            CheckReadError::new(source, format!("response was malformed JSON: {error}"))
        })?;
        let total = response
            .get("total_count")
            .and_then(Value::as_u64)
            .and_then(|count| usize::try_from(count).ok())
            .ok_or_else(|| CheckReadError::new(source, "response had no valid total_count"))?;
        if expected_total.is_some_and(|expected| expected != total) {
            return Err(CheckReadError::new(
                source,
                "total_count changed during pagination",
            ));
        }
        expected_total = Some(total);
        let items = response
            .get(array_field)
            .and_then(Value::as_array)
            .ok_or_else(|| {
                CheckReadError::new(source, format!("response had no {array_field} array"))
            })?;
        for item in items {
            normalized.push(normalize(item).map_err(|error| CheckReadError::new(source, error))?);
        }
        if normalized.len() == total {
            return Ok(normalized);
        }
        if normalized.len() > total || items.is_empty() {
            return Err(CheckReadError::new(
                source,
                "response count did not match total_count",
            ));
        }
    }
    Err(CheckReadError::new(
        source,
        format!("response exceeded {PAGE_LIMIT} pages"),
    ))
}

fn endpoint(repository: &str, sha: &str, source: CheckSource, page: usize) -> String {
    let suffix = match source {
        CheckSource::CheckRuns => "check-runs",
        CheckSource::Statuses => "status",
    };
    format!("repos/{repository}/commits/{sha}/{suffix}?per_page={PAGE_SIZE}&page={page}")
}

fn normalize_check_run(check: &Value) -> Result<Value, String> {
    let name = required_string(check, "name")?;
    let status = required_string(check, "status")?.to_ascii_uppercase();
    let conclusion = match check.get("conclusion") {
        Some(Value::Null) => Value::Null,
        Some(Value::String(value)) => Value::String(value.to_ascii_uppercase()),
        _ => return Err("check run had no valid conclusion".to_owned()),
    };
    let app_slug = check
        .pointer("/app/slug")
        .and_then(Value::as_str)
        .map_or(Value::Null, |slug| Value::String(slug.to_owned()));
    Ok(json!({
        "name": name,
        "conclusion": conclusion,
        "state": status,
        "__typename": "CheckRun",
        "app": {"slug": app_slug},
    }))
}

fn normalize_status(status: &Value) -> Result<Value, String> {
    let name = required_string(status, "context")?;
    let state = required_string(status, "state")?.to_ascii_uppercase();
    Ok(json!({
        "name": name,
        "state": state,
        "__typename": "StatusContext",
    }))
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("item had no valid {field}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_shapes_are_normalized_and_lowercase_conclusions_are_uppercased() {
        let result = read_commit_checks("placeholder/repo", "abc", |endpoint| {
            if endpoint.contains("check-runs") {
                Ok(serde_json::to_vec(&json!({
                    "total_count": 2,
                    "check_runs": [
                        {"name": "test", "status": "completed", "conclusion": "success", "app": {"slug": "github-actions"}},
                        {"name": "queued", "status": "in_progress", "conclusion": null, "app": {"slug": "github-actions"}},
                    ]
                }))
                .unwrap())
            } else {
                Ok(serde_json::to_vec(&json!({
                    "total_count": 1,
                    "statuses": [{"context": "legacy-ci", "state": "pending"}]
                }))
                .unwrap())
            }
        })
        .unwrap();

        assert_eq!(result.checks[0]["conclusion"], "SUCCESS");
        assert_eq!(result.checks[1]["conclusion"], Value::Null);
        assert_eq!(result.checks[1]["state"], "IN_PROGRESS");
        assert_eq!(result.checks[2]["name"], "legacy-ci");
        assert_eq!(result.checks[2]["state"], "PENDING");
        assert!(result.statuses_error.is_none());
    }

    #[test]
    fn unreadable_statuses_leave_readable_check_runs_available() {
        let result = read_commit_checks("placeholder/repo", "abc", |endpoint| {
            if endpoint.contains("check-runs") {
                Ok(serde_json::to_vec(&json!({
                    "total_count": 0,
                    "check_runs": []
                }))
                .unwrap())
            } else {
                Err("HTTP 403: Resource not accessible by integration".to_owned())
            }
        })
        .unwrap();

        assert!(result.checks.is_empty());
        assert!(
            result
                .statuses_error
                .is_some_and(|error| error.to_string().contains("403"))
        );
    }
}
