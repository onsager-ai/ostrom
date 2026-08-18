use std::{collections::BTreeMap, time::Duration};

use ostrom_core::ActionDefinition;
use reqwest::{Url, blocking::Client};
use serde_json::{Value, json};

use crate::{
    ActionFault, ActionOutcome, ActionProvider, PreparedAction,
    process::{exact_keys, invalid_parameters, parameter_timeout},
};

pub struct HttpProvider;

impl ActionProvider for HttpProvider {
    fn domain(&self) -> &'static str {
        "http"
    }

    fn verbs(&self) -> &'static [&'static str] {
        &["get"]
    }

    fn action_definition(&self, verb: &str) -> Option<ActionDefinition> {
        (verb == "get").then(|| ActionDefinition {
            uses: "http/get".to_owned(),
            producer: "ostrom-http".to_owned(),
            default_fresh_for_seconds: 300,
            definition: json!({
                "expect": "status <op> integer | path.to.value|length <op> integer",
                "parameters": ["url", "expect", "timeout"],
                "timeout_default": "30s"
            }),
            source_revision: "http-get-v1".to_owned(),
        })
    }

    fn prepare(
        &self,
        verb: &str,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<Box<dyn PreparedAction>, ActionFault> {
        if verb != "get" || !exact_keys(parameters, &["url", "expect", "timeout"]) {
            return Err(invalid_parameters());
        }
        let url = parameters
            .get("url")
            .and_then(Value::as_str)
            .and_then(|value| Url::parse(value).ok())
            .filter(|value| matches!(value.scheme(), "http" | "https"))
            .ok_or_else(invalid_parameters)?;
        let expect = parameters
            .get("expect")
            .and_then(Value::as_str)
            .ok_or_else(invalid_parameters)
            .and_then(Expectation::parse)?;
        let timeout = parameter_timeout(parameters.get("timeout"))?;
        Ok(Box::new(HttpGet {
            url,
            expect,
            timeout,
        }))
    }
}

struct HttpGet {
    url: Url,
    expect: Expectation,
    timeout: Duration,
}

impl PreparedAction for HttpGet {
    fn execute(&self) -> ActionOutcome {
        // Without a User-Agent GitHub answers 403 regardless of credentials, so an
        // http/get check against it would report a permission fault that does not
        // exist. See ostrom_core::USER_AGENT.
        let Ok(client) = Client::builder()
            .user_agent(ostrom_core::USER_AGENT)
            .timeout(self.timeout)
            .build()
        else {
            return ActionOutcome::Error(ActionFault::new("http_request_error", None));
        };
        let response = match client.get(self.url.clone()).send() {
            Ok(response) => response,
            Err(error) if error.is_timeout() => {
                return ActionOutcome::Error(ActionFault::new("http_timeout", None));
            }
            Err(_) => {
                return ActionOutcome::Error(ActionFault::new("http_request_error", None));
            }
        };
        let status = response.status().as_u16();
        match self.expect.evaluate(status, response) {
            Ok(true) => ActionOutcome::Pass,
            Ok(false) => ActionOutcome::Fail,
            Err(fault) => ActionOutcome::Error(fault),
        }
    }
}

#[derive(Clone)]
enum Expectation {
    Status(Comparison, u64),
    JsonLength(Vec<String>, Comparison, u64),
}

impl Expectation {
    fn parse(source: &str) -> Result<Self, ActionFault> {
        let mut parts = source.split_ascii_whitespace();
        let (Some(subject), Some(operator), Some(expected), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(unsupported_expect());
        };
        let comparison = Comparison::parse(operator).ok_or_else(unsupported_expect)?;
        let expected = expected.parse::<u64>().map_err(|_| unsupported_expect())?;
        if subject == "status" {
            return Ok(Self::Status(comparison, expected));
        }
        let Some(path) = subject.strip_suffix("|length") else {
            return Err(unsupported_expect());
        };
        let segments = path.split('.').map(str::to_owned).collect::<Vec<String>>();
        if segments.is_empty()
            || segments.iter().any(|segment| {
                segment.is_empty()
                    || !segment
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            })
        {
            return Err(unsupported_expect());
        }
        Ok(Self::JsonLength(segments, comparison, expected))
    }

    fn evaluate(
        &self,
        status: u16,
        response: reqwest::blocking::Response,
    ) -> Result<bool, ActionFault> {
        match self {
            Self::Status(comparison, expected) => {
                Ok(comparison.evaluate(u64::from(status), *expected))
            }
            Self::JsonLength(path, comparison, expected) => {
                let body = response.text().map_err(|error| {
                    if error.is_timeout() {
                        ActionFault::new("http_timeout", None)
                    } else {
                        malformed_response()
                    }
                })?;
                let document: Value =
                    serde_json::from_str(&body).map_err(|_| malformed_response())?;
                let mut current = &document;
                for segment in path {
                    let Some(next) = current.as_object().and_then(|object| object.get(segment))
                    else {
                        return Ok(false);
                    };
                    current = next;
                }
                let length = match current {
                    Value::Array(values) => values.len(),
                    Value::Object(values) => values.len(),
                    Value::String(value) => value.chars().count(),
                    _ => return Err(malformed_response()),
                };
                let length = u64::try_from(length).map_err(|_| malformed_response())?;
                Ok(comparison.evaluate(length, *expected))
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Comparison {
    Equal,
    NotEqual,
    Greater,
    GreaterOrEqual,
    Less,
    LessOrEqual,
}

impl Comparison {
    fn parse(source: &str) -> Option<Self> {
        match source {
            "==" => Some(Self::Equal),
            "!=" => Some(Self::NotEqual),
            ">" => Some(Self::Greater),
            ">=" => Some(Self::GreaterOrEqual),
            "<" => Some(Self::Less),
            "<=" => Some(Self::LessOrEqual),
            _ => None,
        }
    }

    fn evaluate(self, actual: u64, expected: u64) -> bool {
        match self {
            Self::Equal => actual == expected,
            Self::NotEqual => actual != expected,
            Self::Greater => actual > expected,
            Self::GreaterOrEqual => actual >= expected,
            Self::Less => actual < expected,
            Self::LessOrEqual => actual <= expected,
        }
    }
}

fn unsupported_expect() -> ActionFault {
    ActionFault::new("unsupported_expect", None)
}

fn malformed_response() -> ActionFault {
    ActionFault::new("http_malformed_response", None)
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    use ostrom_core::{Catalogue, CatalogueEnumeration, CheckDocument, CheckReceipt, CheckVerdict};

    use super::HttpProvider;
    use crate::ActionRegistry;

    fn serve_once(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback fixture");
        let address = listener.local_addr().expect("fixture address");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("fixture request");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("fixture response");
        });
        format!("http://{address}/records")
    }

    fn enumeration(url: &str, expect: &str) -> CatalogueEnumeration {
        let yaml = format!(
            "checks_version: 1\nchecks:\n  request:\n    uses: http/get\n    with:\n      url: {url}\n      expect: {expect:?}\n      timeout: 1s\n"
        );
        CatalogueEnumeration {
            catalogues: vec![Catalogue {
                document: CheckDocument::from_yaml(&yaml).expect("HTTP fixture"),
            }],
            complete: true,
        }
    }

    fn execute(url: &str, expect: &str) -> CheckReceipt {
        let mut registry = ActionRegistry::new();
        registry.register(HttpProvider).expect("HTTP provider");
        registry
            .prepare("request", &enumeration(url, expect))
            .expect("prepared request")
            .execute("request-attempt")
    }

    #[test]
    fn false_expectation_is_a_fail() {
        let receipt = execute(&serve_once(r#"{"records":[]}"#), "records|length > 0");
        assert_eq!(receipt.verdict, Some(CheckVerdict::Fail));
        assert_eq!(receipt.error, None);
    }

    #[test]
    fn true_expectation_is_a_pass() {
        let receipt = execute(
            &serve_once(r#"{"records":[{"id":"fixture"}]}"#),
            "records|length > 0",
        );
        assert_eq!(receipt.verdict, Some(CheckVerdict::Pass));
    }

    #[test]
    fn unreachable_host_is_an_error_not_a_fail() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let address = listener.local_addr().expect("fixture address");
        drop(listener);
        let receipt = execute(&format!("http://{address}/absent"), "status == 200");
        assert_eq!(receipt.verdict, None);
        assert_eq!(receipt.error.as_deref(), Some("http_request_error"));
    }

    #[test]
    fn unsupported_expect_is_refused_by_name_before_the_request() {
        let mut registry = ActionRegistry::new();
        registry.register(HttpProvider).expect("HTTP provider");
        let error = registry
            .prepare(
                "request",
                &enumeration("http://127.0.0.1:1/unused", ".records | length"),
            )
            .err()
            .expect("unsupported expression");
        assert_eq!(error.name(), "unsupported_expect");
    }

    #[test]
    fn invalid_json_is_a_malformed_response_error() {
        let receipt = execute(&serve_once("not-json"), "records|length > 0");
        assert_eq!(receipt.verdict, None);
        assert_eq!(receipt.error.as_deref(), Some("http_malformed_response"));
    }

    #[test]
    fn request_timeout_is_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback fixture");
        let address = listener.local_addr().expect("fixture address");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("fixture request");
            thread::sleep(std::time::Duration::from_millis(100));
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        let mut catalogue = enumeration(&format!("http://{address}/slow"), "status == 200");
        catalogue.catalogues[0]
            .document
            .checks
            .get_mut("request")
            .expect("request fixture")
            .with
            .insert("timeout".to_owned(), serde_json::json!("10ms"));
        let mut registry = ActionRegistry::new();
        registry.register(HttpProvider).expect("HTTP provider");
        let receipt = registry
            .prepare("request", &catalogue)
            .expect("prepared request")
            .execute("timeout-attempt");
        assert_eq!(receipt.verdict, None);
        assert_eq!(receipt.error.as_deref(), Some("http_timeout"));
    }
}
