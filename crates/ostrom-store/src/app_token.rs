use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{SecondsFormat, Utc};
use directories::BaseDirs;
use reqwest::{
    blocking::Client,
    header::{HeaderMap, HeaderValue, USER_AGENT},
};
use rsa::{
    RsaPrivateKey,
    pkcs1::DecodeRsaPrivateKey,
    pkcs1v15::SigningKey,
    pkcs8::DecodePrivateKey,
    signature::{SignatureEncoding as _, Signer as _},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::Sha256;
use thiserror::Error;

use crate::{OstromPaths, TraceAppend, append_trace};

const GITHUB_API: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";

#[derive(Debug, Error)]
pub enum AppTokenError {
    #[error("unscoped token request rejected: {0}")]
    Unscoped(String),
    #[error("caller scope is invalid: {0}")]
    CallerScope(String),
    #[error("credentials unavailable: {0}")]
    Credentials(String),
    #[error("private key unavailable: {0}")]
    PrivateKey(String),
    #[error("JWT signing failed")]
    Signing,
    #[error("GitHub App installation lookup network failure")]
    LookupNetwork,
    #[error("GitHub App is not installed on repository {0}")]
    InstallationMissing(String),
    #[error("GitHub App installation lookup failed with HTTP {0}")]
    LookupHttp(u16),
    #[error("GitHub App installation lookup response was invalid")]
    LookupResponse,
    #[error("scope refused: GitHub App installation lacks requested permission(s): {0}")]
    ScopeRefused(String),
    #[error("GitHub installation token exchange network failure")]
    ExchangeNetwork,
    #[error("scope refused: GitHub App installation rejected the requested permissions")]
    ExchangeScopeRefused,
    #[error("GitHub installation token exchange failed with HTTP {0}")]
    ExchangeHttp(u16),
    #[error("GitHub installation token response was invalid: {0}")]
    ExchangeResponse(&'static str),
    #[error("minted scoped token but could not record its granted scope")]
    Trace,
}

/// The request keeps both scope halves optional so omission is representable
/// and can be rejected before credential or network work begins.
pub(crate) struct AppTokenRequest<'a> {
    pub role: &'a str,
    pub anchor_repository: &'a str,
    pub repositories: Option<&'a str>,
    pub permissions: Option<&'a str>,
}

// No Debug, Display, or Clone: a token can move into a child environment, but
// cannot accidentally enter a formatted error or diagnostic.
pub(crate) struct InstallationToken(String);

impl InstallationToken {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

/// A production command path cannot represent a request with either half of
/// its scope omitted. `AppTokenRequest` retains optional fields only at the
/// validation boundary so the native minter can prove that malformed callers
/// are rejected before credentials or transport are touched.
pub(crate) struct ScopedAppTokenRequest<'a> {
    role: &'a str,
    anchor_repository: &'a str,
    repositories: &'a str,
    permissions: &'a str,
}

impl<'a> ScopedAppTokenRequest<'a> {
    pub(crate) fn new(
        role: &'a str,
        anchor_repository: &'a str,
        repositories: &'a str,
        permissions: &'a str,
    ) -> Self {
        Self {
            role,
            anchor_repository,
            repositories,
            permissions,
        }
    }

    #[cfg(test)]
    pub(crate) fn anchor_repository(&self) -> &str {
        self.anchor_repository
    }

    #[cfg(test)]
    pub(crate) fn role(&self) -> &str {
        self.role
    }

    #[cfg(test)]
    pub(crate) fn repositories(&self) -> &str {
        self.repositories
    }

    #[cfg(test)]
    pub(crate) fn permissions(&self) -> &str {
        self.permissions
    }

    fn into_request(self) -> AppTokenRequest<'a> {
        AppTokenRequest {
            role: self.role,
            anchor_repository: self.anchor_repository,
            repositories: Some(self.repositories),
            permissions: Some(self.permissions),
        }
    }
}

/// Keeps the non-copying production token shape while allowing unit tests to
/// exercise the child boundary without a credential exchange.
pub(crate) enum ScopedInstallationToken {
    Minted(InstallationToken),
    #[cfg(test)]
    Placeholder(String),
}

impl ScopedInstallationToken {
    pub(crate) fn expose(&self) -> &str {
        match self {
            Self::Minted(token) => token.expose(),
            #[cfg(test)]
            Self::Placeholder(value) => value.as_str(),
        }
    }

    #[cfg(test)]
    pub(crate) fn placeholder(value: impl Into<String>) -> Self {
        Self::Placeholder(value.into())
    }
}

pub(crate) trait InstallationTokenMinter {
    fn mint(
        &mut self,
        paths: &OstromPaths,
        request: ScopedAppTokenRequest<'_>,
    ) -> Result<ScopedInstallationToken, AppTokenError>;
}

pub(crate) struct GitHubInstallationTokenMinter;

impl InstallationTokenMinter for GitHubInstallationTokenMinter {
    fn mint(
        &mut self,
        paths: &OstromPaths,
        request: ScopedAppTokenRequest<'_>,
    ) -> Result<ScopedInstallationToken, AppTokenError> {
        mint_installation_token(paths, request.into_request()).map(ScopedInstallationToken::Minted)
    }
}

#[derive(Debug, Error)]
pub(crate) enum AuthenticatedCommandError {
    #[error("GitHub authentication failed: {0}")]
    Authentication(#[source] AppTokenError),
    #[error("authenticated command transport failed: {0}")]
    Transport(String),
}

pub(crate) fn authenticated_output<S: AsRef<std::ffi::OsStr>>(
    paths: &OstromPaths,
    request: ScopedAppTokenRequest<'_>,
    command: &[S],
    minter: &mut dyn InstallationTokenMinter,
) -> Result<Output, AuthenticatedCommandError> {
    // Process-level lifecycle tests cannot inject a Rust trait across the
    // binary boundary. Keep the established explicit override for those
    // hermetic fixtures; the shipped path never resolves or executes a plugin
    // script, and every non-fixture invocation continues through native minting.
    if let Some(executable) = env::var_os("MANDATE_GH_AS_BIN") {
        return Command::new(executable)
            .arg(request.role)
            .arg(request.anchor_repository)
            .arg("--repositories")
            .arg(request.repositories)
            .arg("--permissions")
            .arg(request.permissions)
            .arg("--")
            .args(command)
            .output()
            .map_err(|error| AuthenticatedCommandError::Transport(error.to_string()));
    }
    let token = minter
        .mint(paths, request)
        .map_err(AuthenticatedCommandError::Authentication)?;
    let (program, arguments) = command.split_first().ok_or_else(|| {
        AuthenticatedCommandError::Transport("child command was empty".to_owned())
    })?;
    Command::new(program)
        .args(arguments)
        .env("GH_TOKEN", token.expose())
        .env("GITHUB_TOKEN", token.expose())
        .output()
        .map_err(|error| AuthenticatedCommandError::Transport(error.to_string()))
}

#[derive(Debug)]
struct ValidatedRequest {
    role: String,
    owner: String,
    anchor_repository: String,
    repository_names: Vec<String>,
    permissions: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AppId {
    Integer(u64),
    String(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCredentials {
    app_id: AppId,
    private_key_path: String,
    #[serde(default)]
    installation_id: Option<Value>,
}

#[derive(Debug)]
struct Credentials {
    app_id: u64,
    private_key_path: PathBuf,
}

#[derive(Deserialize)]
struct InstallationResponse {
    id: u64,
    permissions: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct ExchangeResponse {
    token: String,
    permissions: BTreeMap<String, String>,
    repository_selection: String,
}

struct HttpReply {
    status: u16,
    body: Vec<u8>,
}

trait AppTokenTransport {
    fn lookup(&mut self, jwt: &str, repository: &str) -> Result<HttpReply, ()>;
    fn exchange(&mut self, jwt: &str, installation_id: u64, body: Vec<u8>)
    -> Result<HttpReply, ()>;
}

struct ReqwestTransport<'a> {
    client: Client,
    api_base: &'a str,
}

impl AppTokenTransport for ReqwestTransport<'_> {
    fn lookup(&mut self, jwt: &str, repository: &str) -> Result<HttpReply, ()> {
        // Building the header inside reqwest keeps the signed assertion out of
        // the process argument list, unlike passing it to an HTTP subprocess.
        let response = self
            .client
            .get(format!("{}/repos/{repository}/installation", self.api_base))
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .map_err(|_| ())?;
        let status = response.status().as_u16();
        let body = response.bytes().map_err(|_| ())?.to_vec();
        Ok(HttpReply { status, body })
    }

    fn exchange(
        &mut self,
        jwt: &str,
        installation_id: u64,
        body: Vec<u8>,
    ) -> Result<HttpReply, ()> {
        let response = self
            .client
            .post(format!(
                "{}/app/installations/{installation_id}/access_tokens",
                self.api_base
            ))
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .map_err(|_| ())?;
        let status = response.status().as_u16();
        let body = response.bytes().map_err(|_| ())?.to_vec();
        Ok(HttpReply { status, body })
    }
}

#[derive(Serialize)]
struct ExchangeRequest<'a> {
    repositories: &'a [String],
    permissions: &'a BTreeMap<String, String>,
}

pub(crate) fn mint_installation_token(
    paths: &OstromPaths,
    request: AppTokenRequest<'_>,
) -> Result<InstallationToken, AppTokenError> {
    mint_installation_token_against(paths, request, GITHUB_API)
}

fn mint_installation_token_against(
    paths: &OstromPaths,
    request: AppTokenRequest<'_>,
    api_base: &str,
) -> Result<InstallationToken, AppTokenError> {
    mint_installation_token_with_secrets(paths, request, api_base, resolve_secrets_path)
}

fn mint_installation_token_with_secrets(
    paths: &OstromPaths,
    request: AppTokenRequest<'_>,
    api_base: &str,
    secrets_path: impl FnOnce() -> Result<PathBuf, AppTokenError>,
) -> Result<InstallationToken, AppTokenError> {
    // GitHub rejects a request with no User-Agent using 403 and an
    // administrative-rules message, which reads as a permission failure and is
    // not one. reqwest sends no default. See ostrom_core::USER_AGENT.
    let client = Client::builder()
        .default_headers(github_headers())
        .build()
        .map_err(|_| AppTokenError::LookupNetwork)?;
    let mut transport = ReqwestTransport { client, api_base };
    mint_installation_token_with_transport(paths, request, secrets_path, &mut transport)
}

fn github_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(ostrom_core::USER_AGENT),
    );
    headers
}

fn mint_installation_token_with_transport(
    paths: &OstromPaths,
    request: AppTokenRequest<'_>,
    secrets_path: impl FnOnce() -> Result<PathBuf, AppTokenError>,
    transport: &mut impl AppTokenTransport,
) -> Result<InstallationToken, AppTokenError> {
    let request = validate_request(request)?;
    let secrets_path = secrets_path()?;
    let credentials = load_role_credentials(&secrets_path, &request.role)?;
    let jwt = sign_jwt(&credentials)?;
    let lookup = transport
        .lookup(&jwt, &request.anchor_repository)
        .map_err(|_| AppTokenError::LookupNetwork)?;
    if lookup.status == 404 {
        return Err(AppTokenError::InstallationMissing(
            request.anchor_repository,
        ));
    }
    if lookup.status != 200 {
        return Err(AppTokenError::LookupHttp(lookup.status));
    }
    let installation: InstallationResponse =
        serde_json::from_slice(&lookup.body).map_err(|_| AppTokenError::LookupResponse)?;
    if installation.id == 0 {
        return Err(AppTokenError::LookupResponse);
    }

    let refusals = permission_refusals(&request.permissions, &installation.permissions);
    if !refusals.is_empty() {
        // This prefix is intentionally separate from caller-shape and HTTP
        // errors: the operator needs to change the App installation grant.
        return Err(AppTokenError::ScopeRefused(refusals.join(", ")));
    }

    let body = serde_json::to_vec(&ExchangeRequest {
        repositories: &request.repository_names,
        permissions: &request.permissions,
    })
    .map_err(|_| AppTokenError::ExchangeResponse("could not encode request"))?;
    let exchange = transport
        .exchange(&jwt, installation.id, body)
        .map_err(|_| AppTokenError::ExchangeNetwork)?;
    if exchange.status != 201 {
        if matches!(exchange.status, 403 | 422) && response_mentions_permission(&exchange.body) {
            return Err(AppTokenError::ExchangeScopeRefused);
        }
        return Err(AppTokenError::ExchangeHttp(exchange.status));
    }
    let granted: ExchangeResponse = serde_json::from_slice(&exchange.body)
        .map_err(|_| AppTokenError::ExchangeResponse("response body was malformed"))?;
    if granted.token.is_empty() {
        return Err(AppTokenError::ExchangeResponse(
            "response did not contain a token",
        ));
    }
    if granted.repository_selection != "selected" {
        return Err(AppTokenError::ExchangeResponse(
            "selected repository scope was not confirmed",
        ));
    }
    if granted.permissions != request.permissions {
        return Err(AppTokenError::ExchangeResponse(
            "returned permissions differ from the caller request",
        ));
    }

    let repositories = request
        .repository_names
        .iter()
        .map(|name| Value::String(format!("{}/{name}", request.owner)))
        .collect();
    let permissions = request
        .permissions
        .iter()
        .map(|(name, level)| (name.clone(), Value::String(level.clone())))
        .collect();
    // Effective scope is useful evidence; JWTs, tokens, App/installation ids,
    // and key material never belong in durable state.
    let fact = Map::from_iter([
        ("role".to_owned(), Value::String(request.role)),
        ("repositories".to_owned(), Value::Array(repositories)),
        ("permissions".to_owned(), Value::Object(permissions)),
    ]);
    append_trace(
        &paths.trace_file(),
        &TraceAppend {
            ts: chrono::DateTime::<Utc>::from(SystemTime::now())
                .to_rfc3339_opts(SecondsFormat::Secs, true),
            kind: "installation-token-minted".to_owned(),
            fact,
            narration: Map::new(),
        },
    )
    .map_err(|_| AppTokenError::Trace)?;

    Ok(InstallationToken(granted.token))
}

fn validate_request(request: AppTokenRequest<'_>) -> Result<ValidatedRequest, AppTokenError> {
    let repositories = request
        .repositories
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppTokenError::Unscoped("caller must supply --repositories".to_owned()))?;
    let permissions = request
        .permissions
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppTokenError::Unscoped("caller must supply --permissions".to_owned()))?;

    if !valid_role(request.role) {
        return Err(AppTokenError::CallerScope(
            "role must match [a-z][a-z0-9_-]*".to_owned(),
        ));
    }
    let (owner, anchor_name) = parse_repository(request.anchor_repository).ok_or_else(|| {
        AppTokenError::CallerScope("lookup repository must be owner/repository".to_owned())
    })?;
    let mut repository_names = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in repositories.split(',') {
        let name = if let Some((requested_owner, name)) = entry.split_once('/') {
            if requested_owner != owner || name.contains('/') {
                return Err(AppTokenError::CallerScope(format!(
                    "repository {entry} is outside installation owner {owner}"
                )));
            }
            name
        } else {
            entry
        };
        if !valid_repository_part(name) {
            return Err(AppTokenError::CallerScope(format!(
                "invalid repository entry '{entry}'"
            )));
        }
        if !seen.insert(name.to_owned()) {
            return Err(AppTokenError::CallerScope(
                "--repositories contains a duplicate".to_owned(),
            ));
        }
        repository_names.push(name.to_owned());
    }
    if repository_names.is_empty() {
        return Err(AppTokenError::Unscoped(
            "caller must name at least one repository".to_owned(),
        ));
    }
    if !seen.contains(anchor_name) {
        return Err(AppTokenError::CallerScope(format!(
            "lookup repository {} is absent from --repositories",
            request.anchor_repository
        )));
    }
    repository_names.sort();

    let mut permission_map = BTreeMap::new();
    for entry in permissions.split(',') {
        let Some((name, level)) = entry.split_once(':') else {
            return Err(AppTokenError::CallerScope(format!(
                "permission '{entry}' must use permission:level"
            )));
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(AppTokenError::CallerScope(format!(
                "invalid permission name '{name}'"
            )));
        }
        if !matches!(level, "read" | "write") {
            return Err(AppTokenError::CallerScope(format!(
                "permission {name} must request read or write"
            )));
        }
        if permission_map
            .insert(name.to_owned(), level.to_owned())
            .is_some()
        {
            return Err(AppTokenError::CallerScope(format!(
                "permission {name} was supplied more than once"
            )));
        }
    }
    if permission_map.is_empty() {
        return Err(AppTokenError::Unscoped(
            "caller must name at least one permission".to_owned(),
        ));
    }

    Ok(ValidatedRequest {
        role: request.role.to_owned(),
        owner: owner.to_owned(),
        anchor_repository: request.anchor_repository.to_owned(),
        repository_names,
        permissions: permission_map,
    })
}

fn resolve_secrets_path() -> Result<PathBuf, AppTokenError> {
    resolve_secrets_path_with(|name| env::var_os(name))
}

fn resolve_secrets_path_with(
    get: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<PathBuf, AppTokenError> {
    if let Some(path) = get("MANDATE_SECRETS_FILE").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let config = if let Some(path) = get("CLAUDE_CONFIG_DIR").filter(|path| !path.is_empty()) {
        PathBuf::from(path)
    } else if let Some(home) = get("HOME").filter(|path| !path.is_empty()) {
        PathBuf::from(home).join(".claude")
    } else {
        BaseDirs::new()
            .map(|dirs| dirs.home_dir().join(".claude"))
            .ok_or_else(|| {
                AppTokenError::Credentials("could not resolve the secrets path".to_owned())
            })?
    };
    Ok(config.join("ostrom/secrets.yaml"))
}

fn load_role_credentials(path: &Path, role: &str) -> Result<Credentials, AppTokenError> {
    let bytes = fs::read(path).map_err(|_| {
        AppTokenError::Credentials(
            "secrets file is missing or unreadable at the configured path".to_owned(),
        )
    })?;
    let document: serde_yaml::Mapping = serde_yaml::from_slice(&bytes)
        .map_err(|_| AppTokenError::Credentials("secrets file is malformed".to_owned()))?;
    let role_key = serde_yaml::Value::String(role.to_owned());
    let shared_key = serde_yaml::Value::String("shared".to_owned());
    let (credential_name, value) = document
        .get(&role_key)
        .map(|value| (role, value))
        .or_else(|| document.get(&shared_key).map(|value| ("shared", value)))
        .ok_or_else(|| {
            AppTokenError::Credentials(format!(
                "neither {role} nor shared credentials are configured"
            ))
        })?;
    let raw: RawCredentials = serde_yaml::from_value(value.clone()).map_err(|_| {
        AppTokenError::Credentials(format!("could not parse {credential_name} credentials"))
    })?;
    let _ = raw.installation_id;
    let app_id = match raw.app_id {
        AppId::Integer(value) if value > 0 => value,
        AppId::String(value) => value
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                AppTokenError::Credentials(format!(
                    "{credential_name} app_id must be a positive integer"
                ))
            })?,
        AppId::Integer(_) => {
            return Err(AppTokenError::Credentials(format!(
                "{credential_name} app_id must be a positive integer"
            )));
        }
    };
    if raw.private_key_path.is_empty() {
        return Err(AppTokenError::Credentials(format!(
            "missing required {credential_name} field: private_key_path"
        )));
    }
    let private_key_path = expand_tilde(&raw.private_key_path)?;
    Ok(Credentials {
        app_id,
        private_key_path,
    })
}

fn expand_tilde(path: &str) -> Result<PathBuf, AppTokenError> {
    expand_tilde_with(path, || {
        env::var_os("HOME")
            .map(PathBuf::from)
            .or_else(|| BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
    })
}

fn expand_tilde_with(
    path: &str,
    home: impl FnOnce() -> Option<PathBuf>,
) -> Result<PathBuf, AppTokenError> {
    if path != "~" && !path.starts_with("~/") {
        return Ok(PathBuf::from(path));
    }
    let home =
        home().ok_or_else(|| AppTokenError::PrivateKey("could not expand '~'".to_owned()))?;
    if path == "~" {
        Ok(home)
    } else {
        Ok(home.join(&path[2..]))
    }
}

fn sign_jwt(credentials: &Credentials) -> Result<String, AppTokenError> {
    let pem = fs::read_to_string(&credentials.private_key_path).map_err(|_| {
        AppTokenError::PrivateKey("private key file is missing or unreadable".to_owned())
    })?;
    let key = RsaPrivateKey::from_pkcs1_pem(&pem)
        .or_else(|_| RsaPrivateKey::from_pkcs8_pem(&pem))
        .map_err(|_| AppTokenError::PrivateKey("private key PEM is invalid".to_owned()))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| AppTokenError::Signing)?
        .as_secs();
    let iat = now.saturating_sub(60);
    let exp = now.checked_add(540).ok_or(AppTokenError::Signing)?;
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = serde_json::to_vec(&json!({
        "iat": iat,
        "exp": exp,
        "iss": credentials.app_id,
    }))
    .map_err(|_| AppTokenError::Signing)?;
    let payload = URL_SAFE_NO_PAD.encode(claims);
    let signing_input = format!("{header}.{payload}");
    let signature = SigningKey::<Sha256>::new(key).sign(signing_input.as_bytes());
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

fn permission_refusals(
    requested: &BTreeMap<String, String>,
    granted: &BTreeMap<String, String>,
) -> Vec<String> {
    requested
        .iter()
        .filter_map(|(name, level)| {
            let held = granted.get(name).map_or("none", String::as_str);
            (held == "none" || (level == "write" && held != "write"))
                .then(|| format!("{name}:{level} (installation grants {held})"))
        })
        .collect()
}

fn response_mentions_permission(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|message| message.to_ascii_lowercase().contains("permission"))
}

fn parse_repository(repository: &str) -> Option<(&str, &str)> {
    let (owner, name) = repository.split_once('/')?;
    (valid_repository_part(owner) && valid_repository_part(name)).then_some((owner, name))
}

fn valid_repository_part(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

fn valid_role(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::atomic::{AtomicBool, Ordering},
    };

    use rsa::{
        RsaPrivateKey,
        pkcs1::{EncodeRsaPrivateKey, LineEnding},
        pkcs8::EncodePrivateKey,
        rand_core::OsRng,
    };
    use tempfile::tempdir;

    use super::*;

    const TOKEN_SENTINEL: &str = "placeholder-installation-token-sentinel";

    /// GitHub returns a misleading 403 when the User-Agent is absent. Testing
    /// the exact header map consumed by the production client keeps that
    /// incident pinned without opening even a loopback network connection.
    #[test]
    fn the_production_minter_sends_a_user_agent() {
        let headers = github_headers();
        let header = headers
            .get(USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .expect("production client has a User-Agent");
        assert!(
            header.contains("ostrom/"),
            "User-Agent must identify ostrom, got: {header}"
        );
    }

    /// The header-map test above cannot see the wiring. `github_headers()` is
    /// consumed in exactly one place — `.default_headers(...)` on the client
    /// builder — so deleting that call leaves the map correct and the request
    /// bare, which is the same one-indirection gap that made #299's first test
    /// worthless and required #301 to fix it. This one reads the bytes.
    #[test]
    fn the_production_minter_sends_a_user_agent_on_the_wire() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };

        let root = tempdir().expect("temporary minter fixture");
        let (secrets, _) = write_credentials(root.path());

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture socket");
        let port = listener.local_addr().expect("fixture address").port();
        let served = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept lookup");
            // Read until the header block completes. A single `read` returns
            // whatever happened to arrive, which is routinely nothing yet.
            let mut raw = Vec::new();
            let mut buffer = [0_u8; 512];
            while !raw.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => raw.extend_from_slice(&buffer[..read]),
                    Err(error) => panic!("read request: {error}"),
                }
            }
            // The minter's own error path is irrelevant here; the request is
            // the artifact under test.
            let _ = stream
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n");
            String::from_utf8_lossy(&raw).into_owned()
        });

        let base = format!("http://127.0.0.1:{port}");
        let _ = mint_installation_token_with_secrets(
            &paths(root.path()),
            request(Some("placeholder-org/alpha"), Some("metadata:read")),
            &base,
            || Ok(secrets.clone()),
        );

        let request_text = served.join().expect("fixture thread");
        let header = request_text
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("user-agent:"))
            .unwrap_or_else(|| {
                panic!(
                    "no User-Agent header on the wire ({} bytes):\n{request_text}",
                    request_text.len()
                )
            });
        assert!(
            header.contains("ostrom/"),
            "User-Agent must identify ostrom, got: {header}"
        );
    }

    fn paths(root: &Path) -> OstromPaths {
        OstromPaths {
            config: root.to_path_buf(),
            state: root.to_path_buf(),
        }
    }

    fn request<'a>(
        repositories: Option<&'a str>,
        permissions: Option<&'a str>,
    ) -> AppTokenRequest<'a> {
        AppTokenRequest {
            role: "gatekeeper",
            anchor_repository: "placeholder-org/alpha",
            repositories,
            permissions,
        }
    }

    #[derive(Default)]
    struct RecordingMinter {
        requests: Vec<(String, String, String, String)>,
        authentication_failure: bool,
    }

    impl InstallationTokenMinter for RecordingMinter {
        fn mint(
            &mut self,
            _paths: &OstromPaths,
            request: ScopedAppTokenRequest<'_>,
        ) -> Result<ScopedInstallationToken, AppTokenError> {
            self.requests.push((
                request.role.to_owned(),
                request.anchor_repository.to_owned(),
                request.repositories.to_owned(),
                request.permissions.to_owned(),
            ));
            if self.authentication_failure {
                Err(AppTokenError::Credentials(
                    "placeholder credentials unavailable".to_owned(),
                ))
            } else {
                Ok(ScopedInstallationToken::placeholder(TOKEN_SENTINEL))
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn scoped_token_reaches_only_the_child_environment() {
        let root = tempdir().expect("temporary command boundary");
        let mut minter = RecordingMinter::default();
        let output = authenticated_output(
            &paths(root.path()),
            ScopedAppTokenRequest::new(
                "builder",
                "placeholder-org/alpha",
                "placeholder-org/alpha",
                "metadata:read,contents:read",
            ),
            &[
                "/bin/sh",
                "-c",
                "[ \"$GH_TOKEN\" = \"$GITHUB_TOKEN\" ] && [ \"$GH_TOKEN\" = \"placeholder-installation-token-sentinel\" ] && printf boundary-ok",
            ],
            &mut minter,
        )
        .expect("run authenticated child");

        assert!(output.status.success());
        assert_eq!(output.stdout, b"boundary-ok");
        assert!(!String::from_utf8_lossy(&output.stdout).contains(TOKEN_SENTINEL));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(TOKEN_SENTINEL));
        assert_eq!(
            minter.requests,
            [(
                "builder".to_owned(),
                "placeholder-org/alpha".to_owned(),
                "placeholder-org/alpha".to_owned(),
                "metadata:read,contents:read".to_owned(),
            )]
        );
        assert!(
            fs::read_dir(root.path())
                .expect("inspect command boundary")
                .next()
                .is_none(),
            "the command boundary must not persist credentials"
        );
    }

    #[cfg(unix)]
    #[test]
    fn authentication_transport_and_empty_output_are_distinct() {
        let root = tempdir().expect("temporary command faults");
        let scope = || {
            ScopedAppTokenRequest::new(
                "builder",
                "placeholder-org/alpha",
                "placeholder-org/alpha",
                "metadata:read",
            )
        };
        let mut authentication = RecordingMinter {
            authentication_failure: true,
            ..RecordingMinter::default()
        };
        let authentication = authenticated_output(
            &paths(root.path()),
            scope(),
            &["/placeholder/command-must-not-run"],
            &mut authentication,
        )
        .expect_err("authentication must fail before spawn");
        assert!(matches!(
            authentication,
            AuthenticatedCommandError::Authentication(_)
        ));

        let mut transport = RecordingMinter::default();
        let transport = authenticated_output(
            &paths(root.path()),
            scope(),
            &["/placeholder/missing-command"],
            &mut transport,
        )
        .expect_err("missing child is a transport failure");
        assert!(matches!(transport, AuthenticatedCommandError::Transport(_)));

        let mut empty = RecordingMinter::default();
        let empty = authenticated_output(
            &paths(root.path()),
            scope(),
            &["/bin/sh", "-c", "true"],
            &mut empty,
        )
        .expect("empty output is a successful child result");
        assert!(empty.status.success());
        assert!(empty.stdout.is_empty());
    }

    fn write_credentials(root: &Path) -> (PathBuf, String) {
        let key = RsaPrivateKey::new(&mut OsRng, 1024).expect("generate throwaway RSA key");
        let pem = key
            .to_pkcs1_pem(LineEnding::LF)
            .expect("encode throwaway PKCS#1 key")
            .to_string();
        let key_path = root.join("throwaway.pem");
        fs::write(&key_path, &pem).expect("write throwaway key");
        let secrets = root.join("secrets.yaml");
        fs::write(
            &secrets,
            format!(
                "shared:\n  app_id: 12345\n  private_key_path: {}\n",
                key_path.display()
            ),
        )
        .expect("write placeholder credentials");
        (secrets, pem)
    }

    struct FakeTransport {
        lookup: Option<Result<HttpReply, ()>>,
        exchange: Option<Result<HttpReply, ()>>,
        calls: Vec<String>,
        authorization: Vec<String>,
        exchange_body: Option<Vec<u8>>,
    }

    impl FakeTransport {
        fn new(lookup: Result<HttpReply, ()>, exchange: Option<Result<HttpReply, ()>>) -> Self {
            Self {
                lookup: Some(lookup),
                exchange,
                calls: Vec::new(),
                authorization: Vec::new(),
                exchange_body: None,
            }
        }
    }

    impl AppTokenTransport for FakeTransport {
        fn lookup(&mut self, jwt: &str, repository: &str) -> Result<HttpReply, ()> {
            self.calls.push(format!("lookup:{repository}"));
            self.authorization.push(jwt.to_owned());
            self.lookup.take().unwrap_or(Err(()))
        }

        fn exchange(
            &mut self,
            jwt: &str,
            installation_id: u64,
            body: Vec<u8>,
        ) -> Result<HttpReply, ()> {
            self.calls.push(format!("exchange:{installation_id}"));
            self.authorization.push(jwt.to_owned());
            self.exchange_body = Some(body);
            self.exchange.take().unwrap_or(Err(()))
        }
    }

    fn reply(status: u16, body: &str) -> HttpReply {
        HttpReply {
            status,
            body: body.as_bytes().to_vec(),
        }
    }

    fn mint_with_path(
        root: &Path,
        request: AppTokenRequest<'_>,
        secrets: &Path,
        transport: &mut FakeTransport,
    ) -> Result<InstallationToken, AppTokenError> {
        mint_installation_token_with_transport(
            &paths(root),
            request,
            || Ok(secrets.to_path_buf()),
            transport,
        )
    }

    fn mint_error(result: Result<InstallationToken, AppTokenError>) -> AppTokenError {
        match result {
            Ok(_) => panic!("mint unexpectedly succeeded"),
            Err(error) => error,
        }
    }

    #[test]
    fn both_scope_halves_are_required_before_credentials_or_network() {
        for request in [
            request(None, Some("metadata:read")),
            request(Some("alpha"), None),
        ] {
            let credentials_touched = AtomicBool::new(false);
            let mut transport = FakeTransport::new(Err(()), None);
            let error = mint_error(mint_installation_token_with_transport(
                &paths(Path::new("/placeholder-unused")),
                request,
                || {
                    credentials_touched.store(true, Ordering::SeqCst);
                    Err(AppTokenError::Credentials("unexpected read".to_owned()))
                },
                &mut transport,
            ));
            assert!(matches!(error, AppTokenError::Unscoped(_)));
            assert!(!credentials_touched.load(Ordering::SeqCst));
            assert!(transport.calls.is_empty());
        }
    }

    #[test]
    fn repository_scope_requires_one_owner_and_the_anchor() {
        let outside = validate_request(request(
            Some("other-placeholder-org/alpha"),
            Some("metadata:read"),
        ))
        .expect_err("cross-owner request must fail");
        assert!(outside.to_string().starts_with("caller scope is invalid:"));
        let absent = validate_request(request(Some("beta"), Some("metadata:read")))
            .expect_err("anchor omission must fail");
        assert!(absent.to_string().contains("absent from --repositories"));
    }

    #[test]
    fn secrets_override_and_shared_fallback_are_exact() {
        let override_path = PathBuf::from("/placeholder/override/secrets.yaml");
        let resolved = resolve_secrets_path_with(|name| {
            (name == "MANDATE_SECRETS_FILE").then(|| override_path.clone().into_os_string())
        })
        .expect("resolve explicit override");
        assert_eq!(resolved, override_path);

        let root = tempdir().expect("temporary credentials");
        let (secrets, _) = write_credentials(root.path());
        let credentials =
            load_role_credentials(&secrets, "gatekeeper").expect("fall back to shared credentials");
        assert_eq!(credentials.app_id, 12345);

        fs::write(
            &secrets,
            "builder:\n  app_id: 1\n  private_key_path: /placeholder/key\n",
        )
        .expect("replace credentials");
        let error = load_role_credentials(&secrets, "gatekeeper")
            .expect_err("unmatched role without shared must fail");
        assert!(
            error
                .to_string()
                .contains("neither gatekeeper nor shared credentials are configured")
        );
    }

    #[test]
    fn tilde_expansion_and_missing_keys_have_named_errors() {
        assert_eq!(
            expand_tilde_with("~/keys/app.pem", || Some(PathBuf::from(
                "/placeholder-home"
            )))
            .expect("expand tilde"),
            PathBuf::from("/placeholder-home/keys/app.pem")
        );
        let credentials = Credentials {
            app_id: 123,
            private_key_path: PathBuf::from("/placeholder/missing.pem"),
        };
        let error = sign_jwt(&credentials).expect_err("missing key must fail");
        assert_eq!(
            error.to_string(),
            "private key unavailable: private key file is missing or unreadable"
        );
    }

    #[test]
    fn both_github_key_encodings_are_accepted() {
        let root = tempdir().expect("temporary keys");
        let key = RsaPrivateKey::new(&mut OsRng, 1024).expect("generate throwaway RSA key");
        let encodings = [
            key.to_pkcs1_pem(LineEnding::LF)
                .expect("PKCS#1")
                .to_string(),
            key.to_pkcs8_pem(LineEnding::LF)
                .expect("PKCS#8")
                .to_string(),
        ];
        for (index, pem) in encodings.iter().enumerate() {
            let path = root.path().join(format!("key-{index}.pem"));
            fs::write(&path, pem).expect("write throwaway key");
            let jwt = sign_jwt(&Credentials {
                app_id: 123,
                private_key_path: path,
            })
            .expect("sign JWT");
            assert_eq!(jwt.split('.').count(), 3);
        }
    }

    #[test]
    fn installation_grants_are_checked_before_exchange() {
        let root = tempdir().expect("temporary mint fixture");
        let (secrets, _) = write_credentials(root.path());
        let mut transport = FakeTransport::new(
            Ok(reply(200, r#"{"id":77,"permissions":{"metadata":"read"}}"#)),
            None,
        );
        let error = mint_error(mint_with_path(
            root.path(),
            request(Some("alpha"), Some("metadata:read,issues:read")),
            &secrets,
            &mut transport,
        ));
        assert!(error.to_string().starts_with("scope refused:"));
        assert_eq!(transport.calls, ["lookup:placeholder-org/alpha"]);
    }

    #[test]
    fn scoped_exchange_returns_token_and_records_only_scope() {
        let root = tempdir().expect("temporary mint fixture");
        let (secrets, _) = write_credentials(root.path());
        let mut transport = FakeTransport::new(
            Ok(reply(200, r#"{"id":77,"permissions":{"metadata":"read"}}"#)),
            Some(Ok(reply(
                201,
                r#"{"token":"placeholder-installation-token-sentinel","permissions":{"metadata":"read"},"repository_selection":"selected"}"#,
            ))),
        );
        let token = mint_with_path(
            root.path(),
            request(Some("placeholder-org/alpha"), Some("metadata:read")),
            &secrets,
            &mut transport,
        )
        .expect("mint scoped token");
        assert_eq!(token.expose(), TOKEN_SENTINEL);
        assert!(
            transport
                .authorization
                .iter()
                .all(|jwt| jwt.starts_with("eyJ"))
        );
        let claims = transport.authorization[0]
            .split('.')
            .nth(1)
            .and_then(|payload| URL_SAFE_NO_PAD.decode(payload).ok())
            .and_then(|payload| serde_json::from_slice::<Value>(&payload).ok())
            .expect("decode JWT claims");
        assert_eq!(claims["iss"], 12345);
        assert_eq!(
            claims["exp"].as_u64().expect("exp") - claims["iat"].as_u64().expect("iat"),
            600
        );
        assert_eq!(
            transport.exchange_body.as_deref(),
            Some(r#"{"repositories":["alpha"],"permissions":{"metadata":"read"}}"#.as_bytes())
        );
        let trace = fs::read_to_string(root.path().join("sprint.jsonl")).expect("read trace");
        assert!(trace.contains("placeholder-org/alpha"));
        assert!(!trace.contains(TOKEN_SENTINEL));
        assert!(!trace.contains("12345"));
    }

    #[test]
    fn rendered_failures_never_contain_key_material_or_minted_token() {
        let root = tempdir().expect("temporary secret leak fixture");
        let (secrets, pem) = write_credentials(root.path());
        let key_fragment = pem
            .lines()
            .find(|line| !line.starts_with("---"))
            .expect("PEM body fragment");
        let mut errors = Vec::new();

        let malformed_secrets = root.path().join("malformed-secrets.yaml");
        fs::write(&malformed_secrets, format!("{TOKEN_SENTINEL}\n{pem}"))
            .expect("write malformed secrets");
        errors.push(
            load_role_credentials(&malformed_secrets, "gatekeeper")
                .expect_err("malformed credentials"),
        );

        let mut lookup_transport = FakeTransport::new(
            Ok(reply(
                500,
                r#"{"message":"placeholder-installation-token-sentinel"}"#,
            )),
            None,
        );
        errors.push(mint_error(mint_with_path(
            root.path(),
            request(Some("alpha"), Some("metadata:read")),
            &secrets,
            &mut lookup_transport,
        )));

        let mut exchange_transport = FakeTransport::new(
            Ok(reply(200, r#"{"id":77,"permissions":{"metadata":"read"}}"#)),
            Some(Ok(reply(
                403,
                r#"{"message":"placeholder-installation-token-sentinel permission"}"#,
            ))),
        );
        errors.push(mint_error(mint_with_path(
            root.path(),
            request(Some("alpha"), Some("metadata:read")),
            &secrets,
            &mut exchange_transport,
        )));

        let mut mismatch_transport = FakeTransport::new(
            Ok(reply(200, r#"{"id":77,"permissions":{"metadata":"read"}}"#)),
            Some(Ok(reply(
                201,
                r#"{"token":"placeholder-installation-token-sentinel","permissions":{"metadata":"write"},"repository_selection":"selected"}"#,
            ))),
        );
        errors.push(mint_error(mint_with_path(
            root.path(),
            request(Some("alpha"), Some("metadata:read")),
            &secrets,
            &mut mismatch_transport,
        )));

        let blocked_state = root.path().join("blocked-state");
        fs::write(&blocked_state, "not a directory").expect("write blocked trace parent");
        let mut trace_transport = FakeTransport::new(
            Ok(reply(200, r#"{"id":77,"permissions":{"metadata":"read"}}"#)),
            Some(Ok(reply(
                201,
                r#"{"token":"placeholder-installation-token-sentinel","permissions":{"metadata":"read"},"repository_selection":"selected"}"#,
            ))),
        );
        errors.push(mint_error(mint_installation_token_with_transport(
            &paths(&blocked_state),
            request(Some("alpha"), Some("metadata:read")),
            || Ok(secrets.clone()),
            &mut trace_transport,
        )));

        let invalid_key = root.path().join("invalid.pem");
        fs::write(
            &invalid_key,
            format!(
                "{TOKEN_SENTINEL}\n{}",
                pem.replace("BEGIN RSA PRIVATE KEY", "BEGIN BROKEN PRIVATE KEY")
            ),
        )
        .expect("write invalid secret key");
        errors.push(
            sign_jwt(&Credentials {
                app_id: 123,
                private_key_path: invalid_key,
            })
            .expect_err("invalid key"),
        );

        for error in errors {
            let rendered = error.to_string();
            assert!(
                !rendered.contains(TOKEN_SENTINEL),
                "token leaked in {rendered}"
            );
            assert!(!rendered.contains(key_fragment), "key leaked in {rendered}");
            assert!(
                !rendered.contains("BEGIN RSA PRIVATE KEY"),
                "PEM leaked in {rendered}"
            );
        }
    }

    #[test]
    fn caller_scope_grant_and_transport_failures_remain_distinguishable() {
        let malformed = validate_request(request(Some("beta"), Some("metadata:read")))
            .expect_err("malformed caller scope");
        assert!(
            malformed
                .to_string()
                .starts_with("caller scope is invalid:")
        );

        let root = tempdir().expect("temporary failure fixture");
        let (secrets, _) = write_credentials(root.path());
        let mut refused = FakeTransport::new(
            Ok(reply(200, r#"{"id":77,"permissions":{"metadata":"read"}}"#)),
            None,
        );
        let refused = mint_error(mint_with_path(
            root.path(),
            request(Some("alpha"), Some("metadata:read,issues:read")),
            &secrets,
            &mut refused,
        ));
        assert!(refused.to_string().starts_with("scope refused:"));

        let mut network = FakeTransport::new(Err(()), None);
        let network = mint_error(mint_with_path(
            root.path(),
            request(Some("alpha"), Some("metadata:read")),
            &secrets,
            &mut network,
        ));
        assert_eq!(
            network.to_string(),
            "GitHub App installation lookup network failure"
        );
    }
}
