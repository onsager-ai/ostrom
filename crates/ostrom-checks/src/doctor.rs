use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::{OsStr, OsString},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, SecondsFormat, Utc};
use ostrom_core::ActionDefinition;
use serde_json::{Map, Value, json};

use crate::{
    ActionFault, ActionOutcome, ActionProvider, PreparedAction,
    process::{exact_keys, invalid_parameters, parameter_timeout},
};

pub const DOCTOR_CHECKS: &[&str] = &[
    "cli-installed",
    "cli-version",
    "cli-launcher",
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

#[derive(Clone, Debug)]
pub struct DoctorOptions {
    pub plugin_root: PathBuf,
    pub config_dir: PathBuf,
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
}

impl DoctorOptions {
    #[must_use]
    pub fn from_environment(plugin_root: impl Into<PathBuf>) -> Self {
        let cwd = env::current_dir().unwrap_or_default();
        let home = env::var_os("HOME").map_or_else(PathBuf::new, PathBuf::from);
        let config_dir = env::var_os("CLAUDE_CONFIG_DIR").map_or_else(
            || {
                if home.is_absolute() {
                    home.join(".claude")
                } else {
                    cwd.join(&home).join(".claude")
                }
            },
            PathBuf::from,
        );
        Self {
            plugin_root: plugin_root.into(),
            config_dir,
            cwd,
            home,
            env: env::vars_os().collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoctorStatus {
    Ok,
    Warn,
    Fail,
    Defer,
}

impl DoctorStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
            Self::Defer => "DEFER",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorResult {
    pub status: DoctorStatus,
    pub name: &'static str,
    pub detail: String,
    pub remedy: String,
}

impl DoctorResult {
    fn new(
        status: DoctorStatus,
        name: &'static str,
        detail: impl Into<String>,
        remedy: impl Into<String>,
    ) -> Self {
        Self {
            status,
            name,
            detail: detail.into(),
            remedy: remedy.into(),
        }
    }

    #[must_use]
    pub fn format(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.status.as_str(),
            self.name,
            sanitize(&self.detail),
            sanitize(&self.remedy)
        )
    }
}

fn sanitize(value: &str) -> String {
    value.replace(['\r', '\n'], " ").replace('|', "/")
}

struct DoctorContext {
    options: DoctorOptions,
    trace: TraceFile,
    marketplace: Option<MarketplaceInspection>,
}

#[derive(Clone)]
enum TraceFile {
    Missing,
    Content(String),
    Unreadable,
}

impl DoctorContext {
    fn new(options: DoctorOptions) -> Self {
        // The trace grows without bound and several checks scan it. One
        // context-scoped read keeps a doctor run deterministic and pays that
        // I/O once rather than once per trace-backed check.
        let trace_path = options.config_dir.join("ostrom/sprint.jsonl");
        let trace = match fs::read_to_string(&trace_path) {
            Ok(content) => TraceFile::Content(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => TraceFile::Missing,
            Err(_) => TraceFile::Unreadable,
        };
        Self {
            options,
            trace,
            marketplace: None,
        }
    }

    fn env(&self, name: &str) -> Option<&OsStr> {
        self.options
            .env
            .get(OsStr::new(name))
            .map(OsString::as_os_str)
    }

    fn env_text(&self, name: &str) -> Option<&str> {
        self.env(name).and_then(OsStr::to_str)
    }

    fn command(&self, executable: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(executable);
        command
            .env_clear()
            .envs(&self.options.env)
            .current_dir(&self.options.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn git(&self, cwd: &Path, arguments: &[&str]) -> Option<Output> {
        let mut command = self.command("git");
        command.arg("-C").arg(cwd).args(arguments);
        command.output().ok()
    }
}

#[must_use]
pub fn run_doctor(options: DoctorOptions) -> String {
    let mut context = DoctorContext::new(options);
    let mut output = String::new();
    for name in DOCTOR_CHECKS {
        output.push_str(&run_named_check(&mut context, name).format());
        output.push('\n');
    }
    output
}

pub fn run_doctor_check(options: DoctorOptions, name: &str) -> Result<String, ActionFault> {
    if !DOCTOR_CHECKS.contains(&name) {
        return Err(ActionFault::new(
            "doctor_unknown_check",
            Some(format!("unknown doctor check: {name}")),
        ));
    }
    let mut context = DoctorContext::new(options);
    Ok(format!(
        "{}\n",
        run_named_check(&mut context, name).format()
    ))
}

fn run_named_check(context: &mut DoctorContext, name: &str) -> DoctorResult {
    match name {
        "cli-installed" => check_cli_installed(context),
        "cli-version" => check_cli_version(context),
        "cli-launcher" => check_cli_launcher(context),
        "plugin" => check_plugin(context),
        "marketplace" => inspect_marketplace(context).result,
        "plugin-cache-drift" => check_plugin_cache_drift(context),
        "rules-layers" => check_rules_layers(context),
        "touch-durability" => check_touch_durability(context),
        "provider-reachable" => check_provider_reachable(context),
        "dispatch-source-roots" => check_dispatch_source_roots(context),
        "trace-lease" => check_trace_lease(context),
        "work-orders" => check_work_orders(context),
        "builder-pass" => check_role_pass(context, DeliveryRole::Builder),
        "gatekeeper-pass" => check_role_pass(context, DeliveryRole::Gatekeeper),
        "publish" => check_publish(context),
        "environment" => check_environment(context),
        "config-parser" => check_config_parser(),
        _ => unreachable!("validated doctor check name"),
    }
}

pub struct DoctorProvider {
    options: DoctorOptions,
}

impl DoctorProvider {
    #[must_use]
    pub fn new(options: DoctorOptions) -> Self {
        Self { options }
    }

    #[must_use]
    pub fn from_environment(plugin_root: impl Into<PathBuf>) -> Self {
        Self::new(DoctorOptions::from_environment(plugin_root))
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
            source_revision: "doctor-check-v2-native".to_owned(),
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
            options: self.options.clone(),
            check: check.to_owned(),
            timeout,
        }))
    }
}

struct DoctorCheck {
    options: DoctorOptions,
    check: String,
    timeout: std::time::Duration,
}

impl PreparedAction for DoctorCheck {
    fn execute(&self) -> ActionOutcome {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let options = self.options.clone();
        let check = self.check.clone();
        std::thread::spawn(move || {
            let mut context = DoctorContext::new(options);
            let _ = sender.send(run_named_check(&mut context, &check).status);
        });
        let status = match receiver.recv_timeout(self.timeout) {
            Ok(status) => status,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                return ActionOutcome::Error(ActionFault::new("doctor_timeout", None));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return ActionOutcome::Error(ActionFault::new("doctor_execute_error", None));
            }
        };
        match status {
            DoctorStatus::Ok => ActionOutcome::Pass,
            DoctorStatus::Fail => ActionOutcome::Fail,
            DoctorStatus::Warn => ActionOutcome::Error(ActionFault::new("doctor_warn", None)),
            DoctorStatus::Defer => ActionOutcome::Error(ActionFault::new("doctor_defer", None)),
        }
    }
}

const INSTALL_COMMAND: &str = "npm install -g @ostrom/cli";
const UPGRADE_COMMAND: &str = "npm update -g @ostrom/cli";

#[derive(Default)]
struct CliProbe {
    resolved_path: Option<PathBuf>,
    real_path: Option<PathBuf>,
    native_path: Option<PathBuf>,
    node_launcher: bool,
}

fn resolve_on_path(context: &DoctorContext) -> Option<PathBuf> {
    let path = context
        .env("PATH")
        .or_else(|| context.env("Path"))
        .or_else(|| context.env("path"))
        .unwrap_or_default();
    for directory in env::split_paths(path) {
        // A relative PATH segment resolves against the inspected environment's
        // cwd. Using this process's cwd made systemd diagnostics depend on the
        // directory from which doctor itself happened to start.
        let base = if directory.as_os_str().is_empty() {
            context.options.cwd.clone()
        } else if directory.is_absolute() {
            directory
        } else {
            context.options.cwd.join(directory)
        };
        for name in executable_names(context) {
            let candidate = base.join(name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable_names(_context: &DoctorContext) -> Vec<OsString> {
    #[cfg(windows)]
    {
        let extensions = _context
            .env_text("PATHEXT")
            .unwrap_or(".EXE;.CMD;.BAT;.COM");
        let mut names = vec![OsString::from("ostrom")];
        names.extend(
            extensions
                .split(';')
                .filter(|value| !value.is_empty())
                .map(|extension| OsString::from(format!("ostrom{extension}"))),
        );
        names
    }
    #[cfg(not(windows))]
    {
        vec![OsString::from("ostrom")]
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn first_line(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    // Native binaries can be large; launcher classification needs only the
    // shebang and must not read a whole executable into memory.
    let mut bytes = Vec::with_capacity(256);
    file.by_ref().take(256).read_to_end(&mut bytes).ok()?;
    Some(
        String::from_utf8_lossy(&bytes)
            .lines()
            .next()
            .unwrap_or_default()
            .trim_end_matches('\r')
            .to_owned(),
    )
}

fn is_node_launcher(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("#!") else {
        return false;
    };
    let words = rest.split_ascii_whitespace().collect::<Vec<_>>();
    matches!(words.as_slice(), ["/usr/bin/env", "node", ..])
        || matches!(words.as_slice(), ["/usr/bin/env", "-S", "node", ..])
}

fn native_binary(real_path: &Path) -> Option<PathBuf> {
    let manifest_path = real_path.parent()?.join("package.json");
    let manifest: Value = serde_json::from_str(&fs::read_to_string(manifest_path).ok()?).ok()?;
    let platform_key = node_platform_key();
    let package_name = manifest
        .get("ostrom")?
        .get("platformPackages")?
        .get(platform_key)?
        .as_str()?;
    let package_parts = package_name.split('/').collect::<Vec<_>>();
    let mut cursor = real_path.parent()?;
    loop {
        let mut candidate_manifest = cursor.join("node_modules");
        for part in &package_parts {
            candidate_manifest.push(part);
        }
        candidate_manifest.push("package.json");
        if let Ok(source) = fs::read_to_string(&candidate_manifest) {
            let platform: Value = serde_json::from_str(&source).ok()?;
            let main = platform.get("main")?.as_str()?;
            let candidate = candidate_manifest.parent()?.join(main);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
        cursor = cursor.parent()?;
    }
}

fn node_platform_key() -> String {
    let platform = match env::consts::OS {
        "macos" => "darwin",
        value => value,
    };
    let architecture = match env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        value => value,
    };
    format!("{platform}-{architecture}")
}

fn probe_cli(context: &DoctorContext) -> CliProbe {
    let Some(resolved_path) = resolve_on_path(context) else {
        return CliProbe::default();
    };
    let Ok(real_path) = fs::canonicalize(&resolved_path) else {
        return CliProbe {
            resolved_path: Some(resolved_path),
            ..CliProbe::default()
        };
    };
    let node_launcher = first_line(&real_path).is_some_and(|line| is_node_launcher(&line));
    let native_path = node_launcher.then(|| native_binary(&real_path)).flatten();
    CliProbe {
        resolved_path: Some(resolved_path),
        real_path: Some(real_path),
        native_path,
        node_launcher,
    }
}

fn check_cli_installed(context: &DoctorContext) -> DoctorResult {
    if probe_cli(context).resolved_path.is_none() {
        DoctorResult::new(
            DoctorStatus::Fail,
            "cli-installed",
            "ostrom is not installed or is absent from PATH",
            INSTALL_COMMAND,
        )
    } else {
        DoctorResult::new(
            DoctorStatus::Ok,
            "cli-installed",
            "ostrom found on PATH",
            "",
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Vec<String>,
}

fn parse_semver(source: &str) -> Option<SemVer> {
    let source = source.strip_prefix('v').unwrap_or(source);
    let source = if let Some((left, build)) = source.split_once('+') {
        if build.is_empty()
            || !build
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        {
            return None;
        }
        left
    } else {
        source
    };
    let (core, prerelease) = source
        .split_once('-')
        .map_or((source, Vec::new()), |(left, right)| {
            (left, right.split('.').map(str::to_owned).collect())
        });
    let prerelease_source = prerelease.concat();
    if (!prerelease.is_empty() && prerelease_source.is_empty())
        || !prerelease_source
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return None;
    }
    let parts = core.split('.').collect::<Vec<_>>();
    if parts.len() != 3 || parts.iter().any(|part| !valid_numeric_identifier(part)) {
        return None;
    }
    Some(SemVer {
        major: parts[0].parse().ok()?,
        minor: parts[1].parse().ok()?,
        patch: parts[2].parse().ok()?,
        prerelease,
    })
}

fn valid_numeric_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}

fn compare_semver(left: &SemVer, right: &SemVer) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let core = (left.major, left.minor, left.patch).cmp(&(right.major, right.minor, right.patch));
    if core != Ordering::Equal {
        return core;
    }
    match (left.prerelease.is_empty(), right.prerelease.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    for (left_part, right_part) in left.prerelease.iter().zip(&right.prerelease) {
        let order = match (left_part.parse::<u64>(), right_part.parse::<u64>()) {
            (Ok(left_number), Ok(right_number)) => left_number.cmp(&right_number),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => left_part.cmp(right_part),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    left.prerelease.len().cmp(&right.prerelease.len())
}

fn minimum_cli_version(context: &DoctorContext) -> Option<String> {
    let source = fs::read_to_string(
        context
            .options
            .plugin_root
            .join(".claude-plugin/plugin.json"),
    )
    .ok()?;
    serde_json::from_str::<Value>(&source)
        .ok()?
        .get("minimumCliVersion")?
        .as_str()
        .map(str::to_owned)
}

fn reported_version(output: &str) -> Option<String> {
    output.split_ascii_whitespace().find_map(|word| {
        let candidate = word.strip_prefix('v').unwrap_or(word);
        parse_semver(candidate).map(|_| candidate.to_owned())
    })
}

fn check_cli_version(context: &DoctorContext) -> DoctorResult {
    let probe = probe_cli(context);
    let Some(resolved_path) = probe.resolved_path.as_ref() else {
        return DoctorResult::new(
            DoctorStatus::Ok,
            "cli-version",
            "not checked because ostrom is absent",
            "",
        );
    };
    let Some(required) = minimum_cli_version(context) else {
        return DoctorResult::new(
            DoctorStatus::Fail,
            "cli-version",
            "plugin manifest has no valid minimumCliVersion",
            "repair the installed ostrom plugin manifest",
        );
    };
    let Some(required_version) = parse_semver(&required) else {
        return DoctorResult::new(
            DoctorStatus::Fail,
            "cli-version",
            "plugin manifest has no valid minimumCliVersion",
            "repair the installed ostrom plugin manifest",
        );
    };
    let executable = probe.native_path.as_ref().unwrap_or(resolved_path);
    // The npm launcher itself needs Node on PATH. Probing its packaged native
    // binary separates an old CLI from the non-interactive launcher failure
    // that repeatedly broke systemd units.
    let mut command = context.command(executable);
    command.arg("--version");
    let result = output_with_timeout(command, std::time::Duration::from_secs(5));
    let installed = result.as_ref().and_then(|output| {
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
            .status
            .success()
            .then(|| reported_version(&text))
            .flatten()
    });
    let Some(installed) = installed else {
        return DoctorResult::new(
            DoctorStatus::Fail,
            "cli-version",
            format!(
                "ostrom resolves at {}, but its version could not be read",
                resolved_path.display()
            ),
            UPGRADE_COMMAND,
        );
    };
    let Some(installed_version) = parse_semver(&installed) else {
        return DoctorResult::new(
            DoctorStatus::Fail,
            "cli-version",
            format!(
                "ostrom resolves at {}, but its version could not be read",
                resolved_path.display()
            ),
            UPGRADE_COMMAND,
        );
    };
    if compare_semver(&installed_version, &required_version).is_lt() {
        DoctorResult::new(
            DoctorStatus::Fail,
            "cli-version",
            format!("installed ostrom CLI version {installed} is older than required {required}"),
            UPGRADE_COMMAND,
        )
    } else {
        DoctorResult::new(
            DoctorStatus::Ok,
            "cli-version",
            format!("installed version {installed} satisfies required {required}"),
            "",
        )
    }
}

fn output_with_timeout(mut command: Command, timeout: std::time::Duration) -> Option<Output> {
    let mut child = command.spawn().ok()?;
    let deadline = std::time::Instant::now().checked_add(timeout)?;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn check_cli_launcher(context: &DoctorContext) -> DoctorResult {
    let probe = probe_cli(context);
    let Some(resolved_path) = probe.resolved_path else {
        return DoctorResult::new(
            DoctorStatus::Ok,
            "cli-launcher",
            "not checked because ostrom is absent",
            "",
        );
    };
    let _real_path = probe.real_path;
    if !probe.node_launcher {
        return DoctorResult::new(
            DoctorStatus::Ok,
            "cli-launcher",
            "resolved executable is not a Node launcher",
            "",
        );
    }
    if let Some(native_path) = probe.native_path.filter(|path| path.exists()) {
        return DoctorResult::new(
            DoctorStatus::Warn,
            "cli-launcher",
            format!(
                "ostrom resolves to the Node launcher at {}; native binary is {}",
                resolved_path.display(),
                native_path.display()
            ),
            format!(
                "configure non-interactive units to invoke {} directly",
                native_path.display()
            ),
        );
    }
    DoctorResult::new(
        DoctorStatus::Warn,
        "cli-launcher",
        format!(
            "ostrom resolves to the Node launcher at {}, but its native binary could not be resolved",
            resolved_path.display()
        ),
        format!("{INSTALL_COMMAND} (without --no-optional or --omit=optional)"),
    )
}

fn plugin_json_field(source: &str, name: &str) -> String {
    let marker = format!("\"{name}\"");
    let Some(after_name) = source
        .find(&marker)
        .map(|index| &source[index + marker.len()..])
    else {
        return String::new();
    };
    let Some(after_colon) = after_name
        .find(':')
        .map(|index| after_name[index + 1..].trim_start())
    else {
        return String::new();
    };
    let Some(quoted) = after_colon.strip_prefix('"') else {
        return String::new();
    };
    quoted
        .find('"')
        .map(|end| quoted[..end].to_owned())
        .unwrap_or_default()
}

fn plugin_version_at(plugin_root: &Path) -> String {
    fs::read_to_string(plugin_root.join(".claude-plugin/plugin.json"))
        .map(|source| plugin_json_field(&source, "version"))
        .unwrap_or_default()
}

#[derive(Clone)]
struct PluginInstallation {
    install_path: PathBuf,
    loaded_version: String,
    install_path_version: String,
    registry_version: String,
}

enum PluginResolution {
    MissingRegistry(PathBuf),
    PluginAbsent,
    Found(PluginInstallation),
}

fn resolve_plugin_installation(context: &DoctorContext) -> PluginResolution {
    let installed_json = context
        .options
        .config_dir
        .join("plugins/installed_plugins.json");
    if !installed_json.is_file() {
        return PluginResolution::MissingRegistry(installed_json);
    }
    let source = fs::read_to_string(&installed_json).unwrap_or_default();
    let Some(marker) = source.find("\"ostrom@ostrom\"") else {
        return PluginResolution::PluginAbsent;
    };
    // This deliberately preserves the old marker scanner: registry files have
    // changed shape across Claude releases, while the entry-local fields have
    // stayed stable.
    let block = &source[marker..];
    let install_path = PathBuf::from(plugin_json_field(block, "installPath"));
    let recorded_version = plugin_json_field(block, "version");
    let loaded_version = plugin_version_at(&context.options.plugin_root);
    let install_path_version = plugin_version_at(&install_path);
    let registry_version = if install_path_version.is_empty() {
        recorded_version
    } else {
        install_path_version.clone()
    };
    PluginResolution::Found(PluginInstallation {
        install_path,
        loaded_version,
        install_path_version,
        registry_version,
    })
}

fn check_plugin(context: &DoctorContext) -> DoctorResult {
    let installation = match resolve_plugin_installation(context) {
        PluginResolution::MissingRegistry(path) => {
            return DoctorResult::new(
                DoctorStatus::Fail,
                "plugin",
                format!("no installed_plugins.json at {}", path.display()),
                "/plugin install ostrom@ostrom",
            );
        }
        PluginResolution::PluginAbsent => {
            return DoctorResult::new(
                DoctorStatus::Fail,
                "plugin",
                "ostrom@ostrom not present in installed_plugins.json",
                "/plugin install ostrom@ostrom",
            );
        }
        PluginResolution::Found(installation) => installation,
    };
    match (
        installation.loaded_version.as_str(),
        installation.registry_version.as_str(),
    ) {
        (loaded, registry) if !loaded.is_empty() && !registry.is_empty() => {
            if loaded == registry {
                DoctorResult::new(
                    DoctorStatus::Ok,
                    "plugin",
                    format!("installed, loaded version {loaded}"),
                    "",
                )
            } else {
                DoctorResult::new(
                    DoctorStatus::Warn,
                    "plugin",
                    format!("installed, loaded version {loaded}, registry version {registry}"),
                    "restart the session to reconcile the loaded plugin with the registry",
                )
            }
        }
        ("", registry) if !registry.is_empty() => {
            let source = if installation.install_path_version.is_empty() {
                "registry-recorded version"
            } else {
                "registry version"
            };
            DoctorResult::new(
                DoctorStatus::Ok,
                "plugin",
                format!(
                    "installed, version {registry} (loaded plugin.json not readable, using {source})"
                ),
                "",
            )
        }
        (loaded, "") if !loaded.is_empty() => DoctorResult::new(
            DoctorStatus::Warn,
            "plugin",
            format!("installed, loaded version {loaded}, registry version not readable"),
            "restart the session to reconcile the loaded plugin with the registry",
        ),
        _ => DoctorResult::new(
            DoctorStatus::Fail,
            "plugin",
            "ostrom@ostrom entry found but no version could be determined",
            "/plugin install ostrom@ostrom",
        ),
    }
}

#[derive(Clone)]
struct MarketplaceInspection {
    directory: PathBuf,
    clone_available: bool,
    fetch_available: bool,
    result: DoctorResult,
}

fn inspect_marketplace(context: &mut DoctorContext) -> MarketplaceInspection {
    if let Some(inspection) = &context.marketplace {
        return inspection.clone();
    }
    let known_json = context
        .options
        .config_dir
        .join("plugins/known_marketplaces.json");
    let marketplace_dir = context
        .options
        .config_dir
        .join("plugins/marketplaces/ostrom");
    let known_source = fs::read_to_string(&known_json).unwrap_or_default();
    let inspection = if !known_json.is_file() || !json_key_present(&known_source, "ostrom") {
        MarketplaceInspection {
            directory: marketplace_dir,
            clone_available: false,
            fetch_available: false,
            result: DoctorResult::new(
                DoctorStatus::Fail,
                "marketplace",
                "ostrom not registered in known_marketplaces.json",
                "/plugin marketplace add onsager-ai/ostrom",
            ),
        }
    } else if !marketplace_dir.join(".git").is_dir() {
        MarketplaceInspection {
            directory: marketplace_dir.clone(),
            clone_available: false,
            fetch_available: false,
            result: DoctorResult::new(
                DoctorStatus::Fail,
                "marketplace",
                format!(
                    "registered, but no cached clone at {}",
                    marketplace_dir.display()
                ),
                "/plugin marketplace add onsager-ai/ostrom",
            ),
        }
    } else {
        inspect_marketplace_git(context, marketplace_dir)
    };
    context.marketplace = Some(inspection.clone());
    inspection
}

fn json_key_present(source: &str, name: &str) -> bool {
    let marker = format!("\"{name}\"");
    source
        .find(&marker)
        .is_some_and(|index| source[index + marker.len()..].trim_start().starts_with(':'))
}

fn inspect_marketplace_git(context: &DoctorContext, directory: PathBuf) -> MarketplaceInspection {
    let Some(fetch) = context.git(&directory, &["fetch", "origin", "main"]) else {
        return marketplace_fetch_failed(directory, "failed to run git");
    };
    if !fetch.status.success() {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&fetch.stdout),
            String::from_utf8_lossy(&fetch.stderr)
        );
        return marketplace_fetch_failed(directory, text.lines().next().unwrap_or_default());
    }
    if !git_success(
        context,
        &directory,
        &["rev-parse", "--verify", "origin/main"],
    ) {
        return MarketplaceInspection {
            directory,
            clone_available: true,
            fetch_available: true,
            result: DoctorResult::new(
                DoctorStatus::Warn,
                "marketplace",
                "fetched, but origin/main not found (default branch may differ)",
                "",
            ),
        };
    }
    if git_success(
        context,
        &directory,
        &["merge-base", "--is-ancestor", "HEAD", "origin/main"],
    ) {
        return MarketplaceInspection {
            directory,
            clone_available: true,
            fetch_available: true,
            result: DoctorResult::new(
                DoctorStatus::Ok,
                "marketplace",
                "cached clone can fast-forward to origin/main",
                "",
            ),
        };
    }
    if git_success(context, &directory, &["merge-base", "HEAD", "origin/main"]) {
        return MarketplaceInspection {
            directory,
            clone_available: true,
            fetch_available: true,
            result: DoctorResult::new(
                DoctorStatus::Warn,
                "marketplace",
                "cached clone has diverged from origin/main (shared history, not fast-forwardable)",
                "/plugin marketplace update ostrom",
            ),
        };
    }
    MarketplaceInspection {
        directory,
        clone_available: true,
        fetch_available: true,
        result: DoctorResult::new(
            DoctorStatus::Fail,
            "marketplace",
            "cached clone and origin/main have unrelated histories (marketplace was republished from a fresh history)",
            "/plugin marketplace remove ostrom && /plugin marketplace add onsager-ai/ostrom",
        ),
    }
}

fn marketplace_fetch_failed(directory: PathBuf, detail: &str) -> MarketplaceInspection {
    MarketplaceInspection {
        directory,
        clone_available: true,
        fetch_available: false,
        result: DoctorResult::new(
            DoctorStatus::Warn,
            "marketplace",
            format!("cannot verify freshness, git fetch failed (offline?): {detail}"),
            "",
        ),
    }
}

fn git_success(context: &DoctorContext, cwd: &Path, arguments: &[&str]) -> bool {
    context
        .git(cwd, arguments)
        .is_some_and(|output| output.status.success())
}

const SHIPPED_DIRECTORIES: &[&str] = &["skills", "scripts", "hooks", "rules"];
const MARKETPLACE_PLUGIN_ROOT: &str = "plugins/ostrom";

#[derive(Clone, Eq, PartialEq)]
struct Fingerprint {
    mode: String,
    object: String,
}

fn installed_files(plugin_root: &Path) -> std::io::Result<BTreeMap<String, Fingerprint>> {
    let mut files = BTreeMap::new();
    for directory in SHIPPED_DIRECTORIES {
        let path = plugin_root.join(directory);
        match fs::symlink_metadata(&path) {
            Ok(_) => walk_installed(&path, directory, &mut files)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(files)
}

fn walk_installed(
    path: &Path,
    relative: &str,
    files: &mut BTreeMap<String, Fingerprint>,
) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        if relative.split('/').any(|part| part == "node_modules") {
            return Ok(());
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            walk_installed(&entry.path(), &format!("{relative}/{name}"), files)?;
        }
        return Ok(());
    }
    if !metadata.is_file() && !metadata.file_type().is_symlink() {
        return Ok(());
    }
    let (mode, contents) = if metadata.file_type().is_symlink() {
        (
            "120000",
            fs::read_link(path)?
                .to_string_lossy()
                .into_owned()
                .into_bytes(),
        )
    } else {
        (file_mode(&metadata), fs::read(path)?)
    };
    files.insert(
        relative.to_owned(),
        Fingerprint {
            mode: mode.to_owned(),
            object: git_blob_hash(&contents),
        },
    );
    Ok(())
}

fn file_mode(metadata: &fs::Metadata) -> &'static str {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 {
            "100755"
        } else {
            "100644"
        }
    }
    #[cfg(not(unix))]
    {
        "100644"
    }
}

fn git_blob_hash(contents: &[u8]) -> String {
    let mut source = format!("blob {}\0", contents.len()).into_bytes();
    source.extend_from_slice(contents);
    let digest = sha1(&source);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha1(source: &[u8]) -> [u8; 20] {
    let mut message = source.to_vec();
    let bit_len = (message.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());
    let mut state = [
        0x6745_2301_u32,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];
    for chunk in message.chunks_exact(64) {
        let mut words = [0_u32; 80];
        for (index, word) in words.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes(chunk[index * 4..index * 4 + 4].try_into().unwrap());
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (index, word) in words.iter().enumerate() {
            let (function, constant) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(function)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut digest = [0_u8; 20];
    for (index, value) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&value.to_be_bytes());
    }
    digest
}

fn marketplace_files(
    context: &DoctorContext,
    marketplace_dir: &Path,
) -> Option<BTreeMap<String, Fingerprint>> {
    let arguments = [
        "ls-tree",
        "-r",
        "-z",
        "HEAD",
        "--",
        "plugins/ostrom/skills",
        "plugins/ostrom/scripts",
        "plugins/ostrom/hooks",
        "plugins/ostrom/rules",
    ];
    let output = context.git(marketplace_dir, &arguments)?;
    if !output.status.success() {
        return None;
    }
    let mut files = BTreeMap::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let source = String::from_utf8_lossy(record);
        let Some((left, path)) = source.split_once('\t') else {
            continue;
        };
        let fields = left.split_ascii_whitespace().collect::<Vec<_>>();
        if fields.len() != 3 || fields[1] != "blob" {
            continue;
        }
        let Some(relative) = path.strip_prefix(&format!("{MARKETPLACE_PLUGIN_ROOT}/")) else {
            continue;
        };
        if relative.split('/').any(|part| part == "node_modules") {
            continue;
        }
        files.insert(
            relative.to_owned(),
            Fingerprint {
                mode: fields[0].to_owned(),
                object: fields[2].to_owned(),
            },
        );
    }
    Some(files)
}

fn marketplace_version(context: &DoctorContext, marketplace_dir: &Path) -> String {
    let Some(output) = context.git(
        marketplace_dir,
        &["show", "HEAD:plugins/ostrom/.claude-plugin/plugin.json"],
    ) else {
        return String::new();
    };
    if !output.status.success() {
        return String::new();
    }
    plugin_json_field(&String::from_utf8_lossy(&output.stdout), "version")
}

fn check_plugin_cache_drift(context: &mut DoctorContext) -> DoctorResult {
    let installation = match resolve_plugin_installation(context) {
        PluginResolution::MissingRegistry(path) => {
            return DoctorResult::new(
                DoctorStatus::Warn,
                "plugin-cache-drift",
                format!(
                    "cannot compare shipped files: installed plugin registry missing at {}",
                    path.display()
                ),
                "/plugin install ostrom@ostrom",
            );
        }
        PluginResolution::PluginAbsent => {
            return DoctorResult::new(
                DoctorStatus::Warn,
                "plugin-cache-drift",
                "cannot compare shipped files: ostrom@ostrom not present in installed plugin registry",
                "/plugin install ostrom@ostrom",
            );
        }
        PluginResolution::Found(installation) => installation,
    };
    let marketplace = inspect_marketplace(context);
    if !marketplace.clone_available || !marketplace.fetch_available {
        return DoctorResult::new(
            DoctorStatus::Warn,
            "plugin-cache-drift",
            format!(
                "cannot compare shipped files: {}",
                marketplace.result.detail
            ),
            marketplace.result.remedy,
        );
    }
    let installed_version = installation.registry_version;
    let checkout_version = marketplace_version(context, &marketplace.directory);
    if installed_version.is_empty() || checkout_version.is_empty() {
        return DoctorResult::new(
            DoctorStatus::Warn,
            "plugin-cache-drift",
            "cannot compare shipped files: installed or marketplace version is unreadable",
            "reinstall ostrom@ostrom, then restart the session",
        );
    }
    if installed_version != checkout_version {
        return DoctorResult::new(
            DoctorStatus::Warn,
            "plugin-cache-drift",
            format!(
                "versions differ: installed cache {installed_version}, marketplace checkout {checkout_version}"
            ),
            "update and reinstall ostrom@ostrom, then restart the session",
        );
    }
    let installed = match installed_files(&installation.install_path) {
        Ok(files) => files,
        Err(error) => {
            return DoctorResult::new(
                DoctorStatus::Warn,
                "plugin-cache-drift",
                format!("cannot read installed shipped files: {error}"),
                "reinstall ostrom@ostrom, then restart the session",
            );
        }
    };
    let Some(checkout) = marketplace_files(context, &marketplace.directory) else {
        return DoctorResult::new(
            DoctorStatus::Warn,
            "plugin-cache-drift",
            "cannot read shipped files from the marketplace checkout's current commit",
            "/plugin marketplace update ostrom",
        );
    };
    let drift = file_differences(&installed, &checkout);
    if drift.is_empty() {
        DoctorResult::new(
            DoctorStatus::Ok,
            "plugin-cache-drift",
            format!(
                "version {installed_version} and shipped files agree with the marketplace checkout"
            ),
            "",
        )
    } else {
        let shown = drift.iter().take(8).cloned().collect::<Vec<_>>();
        let remaining = drift.len() - shown.len();
        let summary = if remaining == 0 {
            shown.join("; ")
        } else {
            format!("{}; plus {remaining} more", shown.join("; "))
        };
        DoctorResult::new(
            DoctorStatus::Fail,
            "plugin-cache-drift",
            format!("version {installed_version} agrees but shipped files drift: {summary}"),
            "update and reinstall ostrom@ostrom, then restart the session",
        )
    }
}

fn file_differences(
    installed: &BTreeMap<String, Fingerprint>,
    marketplace: &BTreeMap<String, Fingerprint>,
) -> Vec<String> {
    let paths = installed
        .keys()
        .chain(marketplace.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    paths
        .into_iter()
        .filter_map(
            |path| match (installed.get(&path), marketplace.get(&path)) {
                (None, Some(_)) => Some(format!("missing from installed cache: {path}")),
                (Some(_), None) => Some(format!("only in installed cache: {path}")),
                (Some(left), Some(right)) if left.object != right.object => {
                    Some(format!("content differs: {path}"))
                }
                (Some(left), Some(right)) if left.mode != right.mode => {
                    Some(format!("mode differs: {path}"))
                }
                _ => None,
            },
        )
        .collect()
}

#[derive(Clone, Copy)]
struct RuleLayers {
    hook_missing: bool,
    has_user: bool,
    has_repo: bool,
}

fn compute_rules_layers(context: &DoctorContext) -> RuleLayers {
    let hook = context
        .options
        .plugin_root
        .join("hooks/inject-constitution.sh");
    if !hook.is_file() {
        return RuleLayers {
            hook_missing: true,
            has_user: false,
            has_repo: false,
        };
    }
    let mut command = context.command("bash");
    command
        .arg(&hook)
        .env("CLAUDE_PLUGIN_ROOT", &context.options.plugin_root)
        .env("CLAUDE_CONFIG_DIR", &context.options.config_dir);
    let stdout = command
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    RuleLayers {
        hook_missing: false,
        has_user: stdout.contains("<!-- constitution layer: user "),
        has_repo: stdout.contains("<!-- constitution layer: repo "),
    }
}

fn check_rules_layers(context: &DoctorContext) -> DoctorResult {
    let layers = compute_rules_layers(context);
    if layers.hook_missing {
        return DoctorResult::new(
            DoctorStatus::Fail,
            "rules-layers",
            format!(
                "hook not found at {}",
                context
                    .options
                    .plugin_root
                    .join("hooks/inject-constitution.sh")
                    .display()
            ),
            "reinstall the ostrom plugin",
        );
    }
    let mut fired = vec!["shipped"];
    if layers.has_user {
        fired.push("user");
    }
    if layers.has_repo {
        fired.push("repo");
    }
    let summary = if fired.len() == 1 {
        "shipped only".to_owned()
    } else {
        fired.join(" + ")
    };
    let mut notes = Vec::new();
    if context.options.config_dir.join("ostrom/rules.md").is_file() && !layers.has_user {
        notes.push("user layer present but carries no rules yet (by design)");
    }
    if context.options.cwd.join(".ostrom/rules.md").is_file() && !layers.has_repo {
        notes.push("repo layer present but carries no rules yet (by design)");
    }
    DoctorResult::new(
        DoctorStatus::Ok,
        "rules-layers",
        if notes.is_empty() {
            summary
        } else {
            format!("{summary} ({})", notes.join("; "))
        },
        "",
    )
}

#[derive(Clone, Debug, PartialEq)]
enum ConfigValue {
    String(String),
    Bool(bool),
    Number(f64),
    List(Vec<String>),
    Mapping(BTreeMap<String, ConfigValue>),
}

type Config = BTreeMap<String, ConfigValue>;

fn strip_comment(input: &str) -> &str {
    let mut single = false;
    let mut double = false;
    let bytes = input.as_bytes();
    for (index, character) in input.char_indices() {
        match character {
            '\'' if !double => single = !single,
            '"' if !single && (index == 0 || bytes[index - 1] != b'\\') => double = !double,
            '#' if !single && !double => return input[..index].trim_end(),
            _ => {}
        }
    }
    input.trim_end()
}

fn parse_scalar(raw: &str) -> ConfigValue {
    let value = raw.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        return ConfigValue::String(value[1..value.len() - 1].to_owned());
    }
    if let Some(body) = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        let body = body.trim();
        return ConfigValue::List(if body.is_empty() {
            Vec::new()
        } else {
            body.split(',')
                .map(|item| javascript_string(&parse_scalar(item)))
                .collect()
        });
    }
    match value {
        "true" => ConfigValue::Bool(true),
        "false" => ConfigValue::Bool(false),
        _ if yaml_subset_number(value) => ConfigValue::Number(
            value
                .parse::<f64>()
                .expect("validated YAML-subset number parses"),
        ),
        _ => ConfigValue::String(value.to_owned()),
    }
}

fn yaml_subset_number(value: &str) -> bool {
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    if unsigned.is_empty() {
        return false;
    }
    if let Some((whole, fraction)) = unsigned.split_once('.') {
        !fraction.is_empty()
            && whole.bytes().all(|byte| byte.is_ascii_digit())
            && fraction.bytes().all(|byte| byte.is_ascii_digit())
    } else {
        unsigned.bytes().all(|byte| byte.is_ascii_digit())
    }
}

fn javascript_string(value: &ConfigValue) -> String {
    match value {
        ConfigValue::Bool(value) => value.to_string(),
        value => scalar_string(value),
    }
}

fn scalar_string(value: &ConfigValue) -> String {
    match value {
        ConfigValue::String(value) => value.clone(),
        ConfigValue::Bool(true) => "True".to_owned(),
        ConfigValue::Bool(false) => "False".to_owned(),
        ConfigValue::Number(value) => format_number(*value),
        ConfigValue::List(values) => values.join(","),
        ConfigValue::Mapping(_) => "[object Object]".to_owned(),
    }
}

fn format_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn parse_ostrom_yaml(source: &str) -> Config {
    // Ostrom deliberately supports this small YAML shape. Unsupported YAML is
    // ignored rather than guessed at; config-parser reports the exact scope.
    let mut config = Config::new();
    let mut parent: Option<String> = None;
    for original in source.lines() {
        let line = strip_comment(original);
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
        let trimmed = line.trim();
        if indent == 0 {
            let Some((key, raw)) = trimmed.split_once(':') else {
                parent = None;
                continue;
            };
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            let raw = raw.trim();
            if raw.is_empty() {
                config.insert(key.to_owned(), ConfigValue::Mapping(BTreeMap::new()));
                parent = Some(key.to_owned());
            } else {
                config.insert(key.to_owned(), parse_scalar(raw));
                parent = None;
            }
            continue;
        }
        let Some(parent_key) = parent.as_ref() else {
            continue;
        };
        if let Some(raw) = trimmed.strip_prefix("- ") {
            let entry = scalar_string(&parse_scalar(raw));
            match config.get_mut(parent_key) {
                Some(ConfigValue::List(values)) => values.push(entry),
                Some(value) => *value = ConfigValue::List(vec![entry]),
                None => {}
            }
            continue;
        }
        let Some((key, raw)) = trimmed.split_once(':') else {
            continue;
        };
        if key.trim().is_empty() || raw.trim().is_empty() {
            continue;
        }
        if let Some(ConfigValue::Mapping(mapping)) = config.get_mut(parent_key) {
            mapping.insert(key.trim().to_owned(), parse_scalar(raw));
        }
    }
    config
}

fn merge_config(mut base: Config, override_config: Config) -> Config {
    for (key, value) in override_config {
        if let (Some(ConfigValue::Mapping(previous)), ConfigValue::Mapping(next)) =
            (base.get_mut(&key), &value)
        {
            previous.extend(next.clone());
        } else {
            base.insert(key, value);
        }
    }
    base
}

fn load_config(path: &Path) -> Config {
    fs::read_to_string(path)
        .map(|source| parse_ostrom_yaml(&source))
        .unwrap_or_default()
}

fn resolved_config(context: &DoctorContext, filename: &str) -> Config {
    [
        context
            .options
            .plugin_root
            .join(format!("config/{filename}")),
        context
            .options
            .config_dir
            .join(format!("ostrom/{filename}")),
        context.options.cwd.join(format!(".ostrom/{filename}")),
    ]
    .iter()
    .fold(Config::new(), |config, path| {
        merge_config(config, load_config(path))
    })
}

struct TouchConfig {
    provider: String,
    path: String,
    auto_commit: String,
}

fn resolve_touch_config(context: &DoctorContext) -> TouchConfig {
    let config = resolved_config(context, "config.yaml");
    let provider = config
        .get("provider")
        .map(scalar_string)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "file".to_owned());
    let file = match config.get("file") {
        Some(ConfigValue::Mapping(file)) => file,
        _ => &BTreeMap::new(),
    };
    let path = file
        .get("path")
        .map(scalar_string)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "~/.claude/ostrom/touch-log.md".to_owned());
    let auto_commit = file
        .get("auto_commit")
        .map(scalar_string)
        .unwrap_or_else(|| "False".to_owned());
    TouchConfig {
        provider,
        path,
        auto_commit,
    }
}

fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_owned()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

fn inside_git(context: &DoctorContext, path: &Path) -> bool {
    git_success(context, path, &["rev-parse", "--is-inside-work-tree"])
}

fn check_touch_durability(context: &DoctorContext) -> DoctorResult {
    let config = resolve_touch_config(context);
    let expanded_path = expand_tilde(&config.path, &context.options.home);
    let (target_status, target_detail, target_remedy) = match config.provider.as_str() {
        "notion" => (
            DoctorStatus::Ok,
            "provider notion (target is inherently shared)".to_owned(),
            String::new(),
        ),
        "file" => {
            let directory = parent_directory(&expanded_path);
            if inside_git(context, directory) {
                (
                    DoctorStatus::Ok,
                    format!(
                        "file provider, {} is inside a git repo (auto_commit={})",
                        expanded_path.display(),
                        config.auto_commit
                    ),
                    String::new(),
                )
            } else {
                (
                    DoctorStatus::Warn,
                    format!(
                        "file provider, {} is NOT inside a git repo — touches logged here never reach another machine",
                        expanded_path.display()
                    ),
                    "point file.path into a synced repo and set auto_commit: true, or switch provider".to_owned(),
                )
            }
        }
        provider => (
            DoctorStatus::Warn,
            format!("unknown provider '{provider}' (durability undetermined)"),
            "check the resolved touch config's provider value".to_owned(),
        ),
    };
    let user_config = context.options.config_dir.join("ostrom/config.yaml");
    let (config_status, config_detail, config_remedy) = if fs::symlink_metadata(&user_config)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        // A broken symlink is intentionally diagnosed as not versioned.
        let target = fs::canonicalize(&user_config).unwrap_or_default();
        if !target.as_os_str().is_empty() && inside_git(context, parent_directory(&target)) {
            (
                DoctorStatus::Ok,
                "config.yaml is a symlink into a git repo (versioned, syncs across machines)"
                    .to_owned(),
                String::new(),
            )
        } else {
            (
                DoctorStatus::Warn,
                "config.yaml is a symlink, but its target is not inside a git repo".to_owned(),
                "version the symlink target in a private config repo".to_owned(),
            )
        }
    } else if user_config.is_file() {
        (
            DoctorStatus::Warn,
            "config.yaml is a plain machine-local file (will not sync across machines)".to_owned(),
            format!(
                "version it: move it into a private config repo and symlink it back to {}",
                user_config.display()
            ),
        )
    } else {
        (
            DoctorStatus::Ok,
            "no user config.yaml present (shipped defaults only)".to_owned(),
            String::new(),
        )
    };
    let status = if target_status == DoctorStatus::Warn || config_status == DoctorStatus::Warn {
        DoctorStatus::Warn
    } else {
        DoctorStatus::Ok
    };
    DoctorResult::new(
        status,
        "touch-durability",
        format!("target: {target_detail} -- config: {config_detail}"),
        [target_remedy, config_remedy]
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("; "),
    )
}

fn check_provider_reachable(context: &DoctorContext) -> DoctorResult {
    let config = resolve_touch_config(context);
    let expanded_path = expand_tilde(&config.path, &context.options.home);
    if config.provider == "notion" {
        return DoctorResult::new(
            DoctorStatus::Defer,
            "provider-reachable",
            "notion: MCP availability is a session property, not visible to a shell",
            "",
        );
    }
    if config.provider != "file" {
        return DoctorResult::new(
            DoctorStatus::Warn,
            "provider-reachable",
            format!("unknown provider '{}' (undetermined)", config.provider),
            "",
        );
    }
    let directory = parent_directory(&expanded_path).to_owned();
    let mut existing = directory.clone();
    while !existing.exists() && existing.parent().is_some() {
        existing = existing.parent().unwrap().to_owned();
    }
    if writable(&existing) {
        let detail = if existing == directory {
            format!("file: {} is writable", directory.display())
        } else {
            format!(
                "file: {} does not exist yet, nearest existing ancestor {} is writable",
                directory.display(),
                existing.display()
            )
        };
        DoctorResult::new(DoctorStatus::Ok, "provider-reachable", detail, "")
    } else {
        DoctorResult::new(
            DoctorStatus::Fail,
            "provider-reachable",
            format!(
                "file: {} is not writable — /ostrom:touch cannot write its log",
                existing.display()
            ),
            format!(
                "fix permissions on {}, or point file.path elsewhere",
                existing.display()
            ),
        )
    }
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn writable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o222 != 0
    }
    #[cfg(not(unix))]
    {
        !metadata.permissions().readonly()
    }
}

fn check_dispatch_source_roots(context: &DoctorContext) -> DoctorResult {
    let config = resolved_config(context, "mandates.yaml");
    let roots = match config.get("search_roots") {
        Some(ConfigValue::List(values)) => values.len(),
        _ => 0,
    };
    if roots == 0 {
        DoctorResult::new(
            DoctorStatus::Fail,
            "dispatch-source-roots",
            "search_roots is empty; dispatch cannot resolve source repositories",
            "configure search_roots with a parent directory containing the roster checkouts",
        )
    } else {
        DoctorResult::new(
            DoctorStatus::Ok,
            "dispatch-source-roots",
            format!(
                "{roots} search {} configured for dispatch",
                if roots == 1 { "root" } else { "roots" }
            ),
            "",
        )
    }
}

fn now_epoch(context: &DoctorContext) -> i64 {
    if let Some(value) = context
        .env_text("MANDATE_NOW_EPOCH")
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse().ok())
    {
        return value;
    }
    if let Some(value) = context
        .env_text("MANDATE_SWEEP_TIME")
        .and_then(parse_timestamp)
    {
        return value;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn parse_timestamp(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|date| date.timestamp())
}

struct Health {
    status: DoctorStatus,
    detail: String,
    remedy: String,
}

fn trace_health(trace: &TraceFile, now: i64) -> Health {
    let source = match trace {
        TraceFile::Missing => {
            return Health {
                status: DoctorStatus::Warn,
                detail: "trace absent".to_owned(),
                remedy: "run /ostrom:gatekeep and confirm it creates sprint.jsonl".to_owned(),
            };
        }
        TraceFile::Unreadable => {
            return Health {
                status: DoctorStatus::Warn,
                detail: "trace unreadable".to_owned(),
                remedy: "inspect sprint.jsonl and fix its permissions".to_owned(),
            };
        }
        TraceFile::Content(source) => source,
    };
    let mut content_end = source.len();
    while content_end > 0 && source.as_bytes()[content_end - 1] == b'\n' {
        content_end -= 1;
        if content_end > 0 && source.as_bytes()[content_end - 1] == b'\r' {
            content_end -= 1;
        }
    }
    let source = &source[..content_end];
    if source.is_empty() {
        return Health {
            status: DoctorStatus::Warn,
            detail: "trace present but empty".to_owned(),
            remedy: "run /ostrom:gatekeep and confirm it appends a complete pass".to_owned(),
        };
    }
    let line = source.rsplit_once('\n').map_or(source, |(_, line)| line);
    let Ok(record) = serde_json::from_str::<Value>(line) else {
        return Health {
            status: DoctorStatus::Warn,
            detail: "trace last record is unreadable".to_owned(),
            remedy: "inspect sprint.jsonl and repair or remove its malformed last record"
                .to_owned(),
        };
    };
    let Some(object) = record.as_object() else {
        return invalid_trace_shape();
    };
    let keys = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = ["fact", "kind", "narration", "ts"]
        .into_iter()
        .collect::<BTreeSet<_>>();
    let timestamp = object.get("ts").and_then(Value::as_str);
    if keys != expected
        || timestamp.is_none_or(str::is_empty)
        || object
            .get("kind")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || !object.get("fact").is_some_and(Value::is_object)
        || !object.get("narration").is_some_and(Value::is_object)
    {
        return invalid_trace_shape();
    }
    let timestamp = timestamp.unwrap();
    let Some(epoch) = parse_timestamp(timestamp) else {
        return Health {
            status: DoctorStatus::Warn,
            detail: "trace last record has an invalid timestamp".to_owned(),
            remedy: "inspect sprint.jsonl; records must be written by ostrom trace append"
                .to_owned(),
        };
    };
    if now - epoch > 24 * 60 * 60 {
        Health {
            status: DoctorStatus::Warn,
            detail: format!("trace stale, last {timestamp} (older than 24h)"),
            remedy: "run /ostrom:gatekeep and confirm the recurring loop is active".to_owned(),
        }
    } else {
        Health {
            status: DoctorStatus::Ok,
            detail: format!("trace current, last {timestamp}"),
            remedy: String::new(),
        }
    }
}

fn invalid_trace_shape() -> Health {
    Health {
        status: DoctorStatus::Warn,
        detail: "trace last record has an invalid shape".to_owned(),
        remedy: "inspect sprint.jsonl; records must be written by ostrom trace append".to_owned(),
    }
}

fn lease_health(path: &Path, now: i64) -> Health {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Health {
                status: DoctorStatus::Ok,
                detail: "lease idle".to_owned(),
                remedy: String::new(),
            };
        }
        Err(_) => {
            return Health {
                status: DoctorStatus::Warn,
                detail: "lease unreadable".to_owned(),
                remedy: "inspect sprint.lease and fix its permissions".to_owned(),
            };
        }
    };
    let Ok(lease) = serde_json::from_str::<Value>(&source) else {
        return Health {
            status: DoctorStatus::Warn,
            detail: "lease unreadable".to_owned(),
            remedy: "inspect sprint.lease; only ostrom lease may create or remove it".to_owned(),
        };
    };
    let Some(object) = valid_lease(&lease) else {
        return Health {
            status: DoctorStatus::Warn,
            detail: "lease has an invalid shape".to_owned(),
            remedy: "inspect sprint.lease; only ostrom lease may create or remove it".to_owned(),
        };
    };
    let owner = object["owner"].as_str().unwrap();
    let expires_at = json_safe_integer(&object["expires_at"]).expect("validated lease expiry");
    let expiry = DateTime::<Utc>::from_timestamp(expires_at, 0)
        .map(|date| date.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_default();
    if now >= expires_at {
        Health {
            status: DoctorStatus::Warn,
            detail: format!("lease stale for {owner}, expired {expiry}"),
            remedy: "allow the next gatekeeper pass to reclaim the expired lease".to_owned(),
        }
    } else {
        Health {
            status: DoctorStatus::Ok,
            detail: format!("lease held by {owner} until {expiry}"),
            remedy: String::new(),
        }
    }
}

fn valid_lease(value: &Value) -> Option<&Map<String, Value>> {
    let object = value.as_object()?;
    let keys = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = ["expires_at", "owner", "started_at"]
        .into_iter()
        .collect::<BTreeSet<_>>();
    let owner = object.get("owner")?.as_str()?;
    let started = json_safe_integer(object.get("started_at")?)?;
    let expires = json_safe_integer(object.get("expires_at")?)?;
    (keys == expected
        && !owner.is_empty()
        && (0..=8_640_000_000_000).contains(&started)
        && (started..=8_640_000_000_000).contains(&expires))
    .then_some(object)
}

fn json_safe_integer(value: &Value) -> Option<i64> {
    if let Some(value) = value.as_i64() {
        return Some(value);
    }
    let value = value.as_f64()?;
    (value.is_finite()
        && value.fract() == 0.0
        && value.abs() <= 9_007_199_254_740_991.0
        && value >= i64::MIN as f64
        && value <= i64::MAX as f64)
        .then_some(value as i64)
}

fn check_trace_lease(context: &DoctorContext) -> DoctorResult {
    let now = now_epoch(context);
    let trace = trace_health(&context.trace, now);
    let lease = lease_health(&context.options.config_dir.join("ostrom/sprint.lease"), now);
    DoctorResult::new(
        if trace.status == DoctorStatus::Warn || lease.status == DoctorStatus::Warn {
            DoctorStatus::Warn
        } else {
            DoctorStatus::Ok
        },
        "trace-lease",
        format!("{}; {}", trace.detail, lease.detail),
        [trace.remedy, lease.remedy]
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("; "),
    )
}

#[derive(Clone)]
struct DispatchFact {
    item_id: String,
    order_id: String,
    unit_name: String,
    backend: String,
}

fn dispatch_fact(value: &Value) -> Option<DispatchFact> {
    let object = value.as_object()?;
    if json_safe_integer(object.get("schema_version")?)? != 1 {
        return None;
    }
    Some(DispatchFact {
        item_id: nonempty_string(object.get("item_id")?)?,
        order_id: nonempty_string(object.get("order_id")?)?,
        unit_name: nonempty_string(object.get("unit_name")?)?,
        backend: nonempty_string(object.get("backend")?)?,
    })
}

fn nonempty_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn in_flight(source: &str) -> Vec<DispatchFact> {
    let mut dispatched = Vec::<DispatchFact>::new();
    let mut positions = BTreeMap::<String, usize>::new();
    let mut terminal = BTreeSet::new();
    for line in source.lines().filter(|line| !line.is_empty()) {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(object) = record.as_object() else {
            continue;
        };
        let Some(fact) = object.get("fact") else {
            continue;
        };
        match object.get("kind").and_then(Value::as_str) {
            Some("work-dispatched") => {
                if let Some(dispatch) = dispatch_fact(fact) {
                    if let Some(index) = positions.get(&dispatch.order_id).copied() {
                        dispatched[index] = dispatch;
                    } else {
                        positions.insert(dispatch.order_id.clone(), dispatched.len());
                        dispatched.push(dispatch);
                    }
                }
            }
            Some("work-completed" | "work-failed") => {
                if let Some(order_id) = fact.get("order_id").and_then(Value::as_str) {
                    terminal.insert(order_id.to_owned());
                }
            }
            _ => {}
        }
    }
    dispatched
        .into_iter()
        .filter(|fact| !terminal.contains(&fact.order_id))
        .collect()
}

enum UnitState {
    Missing,
    State(String),
    Unknown,
}

fn systemd_unit_state(context: &DoctorContext, unit_name: &str) -> UnitState {
    let executable = context
        .env("MANDATE_SYSTEMCTL_BIN")
        .unwrap_or(OsStr::new("systemctl"));
    let mut command = context.command(executable);
    command.args([
        "--user",
        "show",
        &format!("{unit_name}.service"),
        "--property=ActiveState",
        "--value",
    ]);
    let Ok(output) = command.output() else {
        return UnitState::Unknown;
    };
    if output.status.code() == Some(4) {
        return UnitState::Missing;
    }
    if !output.status.success() {
        return UnitState::Unknown;
    }
    let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if state.is_empty() {
        UnitState::Missing
    } else {
        UnitState::State(state)
    }
}

fn check_work_orders(context: &DoctorContext) -> DoctorResult {
    let TraceFile::Content(source) = &context.trace else {
        return no_work_orders();
    };
    let orders = in_flight(source);
    if orders.is_empty() {
        return no_work_orders();
    }
    let mut faults = Vec::new();
    let mut unknown = Vec::new();
    let visible = orders.iter().map(visible_order).collect::<Vec<_>>();
    for order in &orders {
        if order.backend != "systemd" {
            continue;
        }
        match systemd_unit_state(context, &order.unit_name) {
            UnitState::Unknown => unknown.push(visible_order(order)),
            UnitState::Missing => faults.push(visible_order(order)),
            UnitState::State(state)
                if !["active", "activating", "reloading", "deactivating"]
                    .contains(&state.as_str()) =>
            {
                faults.push(visible_order(order));
            }
            UnitState::State(_) => {}
        }
    }
    if !faults.is_empty() {
        DoctorResult::new(
            DoctorStatus::Fail,
            "work-orders",
            format!(
                "{} in flight; unit exited without terminal row: {}",
                orders.len(),
                faults.join(", ")
            ),
            "inspect the transient unit journal and append work-failed before clearing its per-item lease",
        )
    } else if !unknown.is_empty() {
        DoctorResult::new(
            DoctorStatus::Warn,
            "work-orders",
            format!(
                "{} in flight; could not inspect unit state: {}",
                orders.len(),
                unknown.join(", ")
            ),
            "confirm the user systemd manager is reachable and inspect the transient unit",
        )
    } else {
        DoctorResult::new(
            DoctorStatus::Ok,
            "work-orders",
            format!("{} in flight: {}", orders.len(), visible.join(", ")),
            "",
        )
    }
}

fn visible_order(order: &DispatchFact) -> String {
    format!("{} ({})", order.item_id, order.unit_name)
}

fn no_work_orders() -> DoctorResult {
    DoctorResult::new(
        DoctorStatus::Ok,
        "work-orders",
        "no work orders in flight",
        "",
    )
}

#[derive(Clone, Copy)]
enum DeliveryRole {
    Builder,
    Gatekeeper,
}

impl DeliveryRole {
    fn name(self) -> &'static str {
        match self {
            Self::Builder => "builder",
            Self::Gatekeeper => "gatekeeper",
        }
    }

    fn cadence(self) -> i64 {
        match self {
            Self::Builder => 3,
            Self::Gatekeeper => 1,
        }
    }

    fn skill(self) -> &'static str {
        match self {
            Self::Builder => "/ostrom:work",
            Self::Gatekeeper => "/ostrom:gatekeep",
        }
    }
}

fn recent_role_pass_ended(source: &str, role: DeliveryRole) -> Vec<Value> {
    source
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| {
            record.get("kind").and_then(Value::as_str) == Some("pass-ended")
                && record.get("fact").is_some_and(Value::is_object)
                && record
                    .get("fact")
                    .and_then(|fact| fact.get("owner"))
                    .and_then(Value::as_str)
                    .is_some_and(|owner| owner.starts_with(&format!("{}-", role.name())))
        })
        .take(3)
        .collect()
}

fn format_age(age_seconds: i64) -> String {
    let minutes = (age_seconds / 60).max(0);
    if minutes < 60 {
        format!("{minutes}m")
    } else {
        let hours = minutes / 60;
        let minutes = minutes % 60;
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{minutes}m")
        }
    }
}

fn check_role_pass(context: &DoctorContext, role: DeliveryRole) -> DoctorResult {
    let role_name = role.name();
    let check_name = match role {
        DeliveryRole::Builder => "builder-pass",
        DeliveryRole::Gatekeeper => "gatekeeper-pass",
    };
    let source = match &context.trace {
        TraceFile::Missing => {
            return DoctorResult::new(
                DoctorStatus::Warn,
                check_name,
                format!("no {role_name} pass ever recorded"),
                format!("run {} and confirm it records pass-ended", role.skill()),
            );
        }
        TraceFile::Unreadable => {
            return DoctorResult::new(
                DoctorStatus::Warn,
                check_name,
                format!("{role_name} pass history is unreadable"),
                "inspect sprint.jsonl and fix its permissions",
            );
        }
        TraceFile::Content(source) => source,
    };
    let recent = recent_role_pass_ended(source, role);
    let Some(record) = recent.first() else {
        return DoctorResult::new(
            DoctorStatus::Warn,
            check_name,
            format!("no {role_name} pass ever recorded"),
            format!("run {} and confirm it records pass-ended", role.skill()),
        );
    };
    let timestamp = record.get("ts").and_then(Value::as_str);
    let Some(timestamp_epoch) = timestamp.and_then(parse_timestamp) else {
        return DoctorResult::new(
            DoctorStatus::Warn,
            check_name,
            format!("last {role_name} pass has an invalid timestamp"),
            "inspect sprint.jsonl; records must be written by ostrom trace append",
        );
    };
    let timestamp = timestamp.unwrap();
    let age_seconds = now_epoch(context) - timestamp_epoch;
    let age = format_age(age_seconds);
    // One no-op can be a contended lease or a disarmed mid-window wake. Three
    // consecutive no-ops mean the timer is alive but the protocol has stopped
    // taking ownership, the production failure that the age check cannot see.
    if recent.len() == 3
        && recent
            .iter()
            .all(|record| pass_outcome(record) == Some("no-op"))
    {
        return DoctorResult::new(
            DoctorStatus::Fail,
            check_name,
            format!(
                "{role_name} loop has produced 3 consecutive no-op passes, last {timestamp} (age {age})"
            ),
            format!(
                "inspect pass-runs/{role_name} transcripts; the loop is running but the protocol never takes ownership"
            ),
        );
    }
    // Repeated protocol-owned failures are the same dead loop one layer down;
    // fresh wrapper timestamps must not make them look healthy.
    if recent.len() == 3
        && recent
            .iter()
            .all(|record| pass_outcome(record) == Some("failed"))
    {
        return DoctorResult::new(
            DoctorStatus::Fail,
            check_name,
            format!(
                "{role_name} loop has produced 3 consecutive failed passes, last {timestamp} (age {age})"
            ),
            format!(
                "inspect pass-runs/{role_name} transcripts; the protocol takes ownership but does not complete"
            ),
        );
    }
    if age_seconds > role.cadence() * 60 * 60 {
        DoctorResult::new(
            DoctorStatus::Warn,
            check_name,
            format!(
                "{role_name} pass stale, last {timestamp} (age {age}; older than {}h cadence)",
                role.cadence()
            ),
            format!("confirm ostrom-{role_name}-pass.timer is active and loop-armed is present"),
        )
    } else {
        DoctorResult::new(
            DoctorStatus::Ok,
            check_name,
            format!(
                "{role_name} pass current, last {timestamp} (age {age}; {}h cadence)",
                role.cadence()
            ),
            "",
        )
    }
}

fn pass_outcome(record: &Value) -> Option<&str> {
    record.get("fact")?.get("outcome")?.as_str()
}

fn check_publish(context: &DoctorContext) -> DoctorResult {
    let publish_dir = context.env("MANDATE_PUBLISH_DIR").map_or_else(
        || context.options.config_dir.join("ostrom/publish"),
        PathBuf::from,
    );
    let manifest_path = publish_dir.join("manifest.json");
    let source = match fs::read_to_string(&manifest_path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return DoctorResult::new(
                DoctorStatus::Warn,
                "publish",
                "no publish has been recorded",
                "run mandate publish.sh and confirm the state branch is reachable",
            );
        }
        Err(_) => {
            return DoctorResult::new(
                DoctorStatus::Warn,
                "publish",
                "publish manifest is unreadable",
                "inspect the cached publish clone and repair or recreate it",
            );
        }
    };
    let Ok(manifest) = serde_json::from_str::<Value>(&source) else {
        return DoctorResult::new(
            DoctorStatus::Warn,
            "publish",
            "publish manifest is unreadable",
            "inspect the cached publish clone and repair or recreate it",
        );
    };
    let Some(object) = manifest.as_object() else {
        return DoctorResult::new(
            DoctorStatus::Warn,
            "publish",
            "publish manifest is malformed",
            "run mandate publish.sh to regenerate the cached record tree",
        );
    };
    let published_at = object.get("published_at").and_then(Value::as_str);
    let published_epoch = published_at.and_then(parse_timestamp);
    let cadence = object
        .get("expected_sweep_interval_hours")
        .and_then(json_safe_integer)
        .filter(|value| *value > 0);
    let (Some(published_at), Some(published_epoch), Some(cadence)) =
        (published_at, published_epoch, cadence)
    else {
        return DoctorResult::new(
            DoctorStatus::Warn,
            "publish",
            "publish manifest has invalid cadence or timestamp",
            "run mandate publish.sh to regenerate the cached record tree",
        );
    };
    if now_epoch(context) - published_epoch > cadence * 60 * 60 {
        DoctorResult::new(
            DoctorStatus::Warn,
            "publish",
            format!("publish stale, last {published_at} (older than {cadence}h cadence)"),
            "run mandate publish.sh and confirm the state branch is reachable",
        )
    } else {
        DoctorResult::new(
            DoctorStatus::Ok,
            "publish",
            format!("publish current, last {published_at} ({cadence}h cadence)"),
            "",
        )
    }
}

fn check_environment(context: &DoctorContext) -> DoctorResult {
    if context.env("CLAUDE_CODE_REMOTE").is_none() {
        DoctorResult::new(DoctorStatus::Ok, "environment", "local", "")
    } else if compute_rules_layers(context).has_user {
        DoctorResult::new(
            DoctorStatus::Ok,
            "environment",
            "cloud, user rules layer resolved",
            "",
        )
    } else {
        DoctorResult::new(
            DoctorStatus::Warn,
            "environment",
            "cloud session, no user rules layer resolved (private layer absent)",
            "provide the private layer's credentials/config for this environment",
        )
    }
}

fn check_config_parser() -> DoctorResult {
    DoctorResult::new(
        DoctorStatus::Ok,
        "config-parser",
        "used the built-in ostrom-shape parser (top-level scalars, one level of nesting, inline lists, and comments; the values behind touch-durability/provider-reachable are authoritative for this supported config shape; a DEFER line is still resolved by the caller)",
        "",
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
        process::Command,
    };

    use ostrom_core::{Catalogue, CatalogueEnumeration, CheckDocument, CheckVerdict};
    use tempfile::{TempDir, tempdir};

    use super::{
        DOCTOR_CHECKS, DoctorOptions, DoctorProvider, git_blob_hash, parse_ostrom_yaml, run_doctor,
        run_doctor_check,
    };
    use crate::ActionRegistry;

    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};

    struct Fixture {
        root: TempDir,
        plugin_root: PathBuf,
        config_dir: PathBuf,
        cwd: PathBuf,
        home: PathBuf,
        bin: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempdir().expect("temporary doctor root");
            let plugin_root = root.path().join("plugin");
            let config_dir = root.path().join("config");
            let cwd = root.path().join("project");
            let home = root.path().join("home");
            let bin = root.path().join("bin");
            for path in [&plugin_root, &config_dir, &cwd, &home, &bin] {
                fs::create_dir_all(path).expect("fixture directory");
            }
            fs::create_dir_all(plugin_root.join(".claude-plugin")).unwrap();
            fs::write(
                plugin_root.join(".claude-plugin/plugin.json"),
                r#"{"version":"1.30.15","minimumCliVersion":"0.9.0"}"#,
            )
            .unwrap();
            Self {
                root,
                plugin_root,
                config_dir,
                cwd,
                home,
                bin,
            }
        }

        fn options(&self) -> DoctorOptions {
            DoctorOptions {
                plugin_root: self.plugin_root.clone(),
                config_dir: self.config_dir.clone(),
                cwd: self.cwd.clone(),
                home: self.home.clone(),
                env: BTreeMap::from([
                    (OsString::from("PATH"), self.bin.clone().into_os_string()),
                    (
                        OsString::from("MANDATE_NOW_EPOCH"),
                        OsString::from("1785542400"),
                    ),
                ]),
            }
        }

        #[cfg(unix)]
        fn executable(&self, name: &str, source: &str) -> PathBuf {
            let path = self.bin.join(name);
            fs::write(&path, source).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
    }

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

    #[test]
    fn git_blob_hash_matches_the_git_object_format() {
        assert_eq!(
            git_blob_hash(b"test content\n"),
            "d670460b4b4aece5915caf5c68d12f560a9fe3e4"
        );
    }

    #[test]
    fn supported_yaml_shape_preserves_python_style_booleans() {
        let parsed = parse_ostrom_yaml(
            "provider: file # note\nbuckets: [freezable, \"needs review\"]\nfile:\n  path: \"~/touch # literal.md\" # note\n  auto_commit: true\n",
        );
        assert!(
            matches!(parsed.get("provider"), Some(super::ConfigValue::String(value)) if value == "file")
        );
        assert!(
            matches!(parsed.get("buckets"), Some(super::ConfigValue::List(values)) if values == &["freezable", "needs review"])
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_cli_reports_the_remedy_without_running_npm() {
        let fixture = Fixture::new();
        let marker = fixture.root.path().join("npm-was-run");
        fixture.executable("npm", &format!("#!/bin/sh\ntouch '{}'\n", marker.display()));
        assert_eq!(
            run_doctor_check(fixture.options(), "cli-installed").unwrap(),
            "FAIL|cli-installed|ostrom is not installed or is absent from PATH|npm install -g @ostrom/cli\n"
        );
        assert!(!marker.exists(), "doctor remedies must never be executed");
    }

    #[cfg(unix)]
    #[test]
    fn semantic_versions_compare_numerically() {
        let fixture = Fixture::new();
        fixture.executable("ostrom", "#!/bin/sh\nprintf 'ostrom 0.10.0\\n'\n");
        assert_eq!(
            run_doctor_check(fixture.options(), "cli-version").unwrap(),
            "OK|cli-version|installed version 0.10.0 satisfies required 0.9.0|\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn relative_path_is_resolved_against_the_inspected_cwd() {
        let fixture = Fixture::new();
        let relative_bin = fixture.cwd.join("relative-bin");
        fs::create_dir(&relative_bin).unwrap();
        let executable = relative_bin.join("ostrom");
        fs::write(&executable, "#!/bin/sh\nprintf 'ostrom 0.9.0\\n'\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let mut options = fixture.options();
        options.env.insert("PATH".into(), "relative-bin".into());
        assert_eq!(
            run_doctor_check(options, "cli-installed").unwrap(),
            "OK|cli-installed|ostrom found on PATH|\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn node_launcher_reports_the_packaged_native_binary() {
        let fixture = Fixture::new();
        let package = fixture.root.path().join("global/@ostrom/cli");
        let platform_key = super::node_platform_key();
        let platform_name = format!("cli-{platform_key}");
        let platform = package.join(format!("node_modules/@ostrom/{platform_name}"));
        fs::create_dir_all(&platform).unwrap();
        fs::write(
            package.join("package.json"),
            format!(
                r#"{{"ostrom":{{"platformPackages":{{"{platform_key}":"@ostrom/{platform_name}"}}}}}}"#
            ),
        )
        .unwrap();
        fs::write(platform.join("package.json"), r#"{"main":"ostrom"}"#).unwrap();
        let native = platform.join("ostrom");
        fs::write(&native, "#!/bin/sh\nprintf 'ostrom 0.9.0\\n'\n").unwrap();
        fs::set_permissions(&native, fs::Permissions::from_mode(0o755)).unwrap();
        let launcher = package.join("bin.js");
        fs::write(&launcher, "#!/usr/bin/env node\nprocess.exit(99);\n").unwrap();
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&launcher, fixture.bin.join("ostrom")).unwrap();
        let output = run_doctor_check(fixture.options(), "cli-launcher").unwrap();
        assert_eq!(
            output,
            format!(
                "WARN|cli-launcher|ostrom resolves to the Node launcher at {}; native binary is {}|configure non-interactive units to invoke {} directly\n",
                fixture.bin.join("ostrom").display(),
                native.display(),
                native.display()
            )
        );
    }

    #[test]
    fn every_check_is_individually_addressable_through_the_native_provider() {
        let fixture = Fixture::new();
        let mut registry = ActionRegistry::new();
        registry
            .register(DoctorProvider::new(fixture.options()))
            .expect("native doctor provider");
        for check in DOCTOR_CHECKS {
            let receipt = registry
                .prepare("doctor-fixture", &catalogue(check))
                .unwrap_or_else(|error| panic!("{check} did not prepare: {error}"))
                .execute(&format!("{check}-attempt"));
            assert!(
                receipt.verdict.is_some() || receipt.error.is_some(),
                "{check} did not execute"
            );
        }
    }

    #[test]
    fn warn_and_defer_remain_named_provider_errors() {
        let fixture = Fixture::new();
        let mut registry = ActionRegistry::new();
        registry
            .register(DoctorProvider::new(fixture.options()))
            .unwrap();
        let warn = registry
            .prepare("doctor-fixture", &catalogue("publish"))
            .unwrap()
            .execute("warn-attempt");
        assert_eq!(warn.verdict, None);
        assert_eq!(warn.error.as_deref(), Some("doctor_warn"));

        fs::create_dir_all(fixture.config_dir.join("ostrom")).unwrap();
        fs::write(
            fixture.config_dir.join("ostrom/config.yaml"),
            "provider: notion\n",
        )
        .unwrap();
        let defer = registry
            .prepare("doctor-fixture", &catalogue("provider-reachable"))
            .unwrap()
            .execute("defer-attempt");
        assert_eq!(defer.error.as_deref(), Some("doctor_defer"));
    }

    #[test]
    fn complete_report_has_one_line_per_check_and_creates_nothing() {
        let fixture = Fixture::new();
        let missing = fixture.root.path().join("missing-config");
        let mut options = fixture.options();
        options.config_dir = missing.clone();
        let report = run_doctor(options);
        assert_eq!(report.lines().count(), DOCTOR_CHECKS.len());
        assert!(!missing.exists());
    }

    #[test]
    fn isolated_machine_report_is_byte_exact() {
        let fixture = Fixture::new();
        let report = run_doctor(fixture.options());
        let expected = format!(
            concat!(
                "FAIL|cli-installed|ostrom is not installed or is absent from PATH|npm install -g @ostrom/cli\n",
                "OK|cli-version|not checked because ostrom is absent|\n",
                "OK|cli-launcher|not checked because ostrom is absent|\n",
                "FAIL|plugin|no installed_plugins.json at {config}/plugins/installed_plugins.json|/plugin install ostrom@ostrom\n",
                "FAIL|marketplace|ostrom not registered in known_marketplaces.json|/plugin marketplace add onsager-ai/ostrom\n",
                "WARN|plugin-cache-drift|cannot compare shipped files: installed plugin registry missing at {config}/plugins/installed_plugins.json|/plugin install ostrom@ostrom\n",
                "FAIL|rules-layers|hook not found at {plugin}/hooks/inject-constitution.sh|reinstall the ostrom plugin\n",
                "WARN|touch-durability|target: file provider, {home}/.claude/ostrom/touch-log.md is NOT inside a git repo — touches logged here never reach another machine -- config: no user config.yaml present (shipped defaults only)|point file.path into a synced repo and set auto_commit: true, or switch provider\n",
                "OK|provider-reachable|file: {home}/.claude/ostrom does not exist yet, nearest existing ancestor {home} is writable|\n",
                "FAIL|dispatch-source-roots|search_roots is empty; dispatch cannot resolve source repositories|configure search_roots with a parent directory containing the roster checkouts\n",
                "WARN|trace-lease|trace absent; lease idle|run /ostrom:gatekeep and confirm it creates sprint.jsonl\n",
                "OK|work-orders|no work orders in flight|\n",
                "WARN|builder-pass|no builder pass ever recorded|run /ostrom:work and confirm it records pass-ended\n",
                "WARN|gatekeeper-pass|no gatekeeper pass ever recorded|run /ostrom:gatekeep and confirm it records pass-ended\n",
                "WARN|publish|no publish has been recorded|run mandate publish.sh and confirm the state branch is reachable\n",
                "OK|environment|local|\n",
                "OK|config-parser|used the built-in ostrom-shape parser (top-level scalars, one level of nesting, inline lists, and comments; the values behind touch-durability/provider-reachable are authoritative for this supported config shape; a DEFER line is still resolved by the caller)|\n"
            ),
            config = fixture.config_dir.display(),
            plugin = fixture.plugin_root.display(),
            home = fixture.home.display(),
        );
        assert_eq!(report, expected);
    }

    #[test]
    fn provider_passes_failures_as_verdicts() {
        let fixture = Fixture::new();
        let mut registry = ActionRegistry::new();
        registry
            .register(DoctorProvider::new(fixture.options()))
            .unwrap();
        let receipt = registry
            .prepare("doctor-fixture", &catalogue("cli-installed"))
            .unwrap()
            .execute("fail-attempt");
        assert_eq!(receipt.verdict, Some(CheckVerdict::Fail));
    }

    #[test]
    fn tests_never_point_at_live_runtime_state() {
        let fixture = Fixture::new();
        assert!(fixture.config_dir.starts_with(fixture.root.path()));
        assert!(fixture.home.starts_with(fixture.root.path()));
    }

    #[allow(dead_code)]
    fn run_git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }
}
