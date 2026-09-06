use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::RunCeilings;

const RECONCILER_SERVICE: &str = "ostrom-up.service";
const RECONCILER_TIMER: &str = "ostrom-up.timer";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopUnit {
    pub name: String,
    pub contents: String,
}

/// Caller-owned naming used when rendering loop units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopUnitGeneratorConfig {
    pub generated_header: String,
    pub unit_prefix: String,
}

/// Environment-variable names used to pass each resolved ceiling to a loop process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CeilingEnvironmentNames {
    pub concurrent: Option<String>,
    pub spend_usd: Option<String>,
    pub tokens: Option<String>,
}

/// A systemd loop already resolved into harness-facing process inputs.
#[derive(Debug, Clone, PartialEq)]
pub struct LoopUnitDeclaration {
    pub name: String,
    pub argv: Vec<String>,
    pub on_calendars: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub ceiling_environment: CeilingEnvironmentNames,
    pub ceilings: RunCeilings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopUnitDrift {
    pub missing: Vec<String>,
    pub changed: Vec<String>,
    pub unexpected: Vec<String>,
}

impl LoopUnitDrift {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty() && self.changed.is_empty() && self.unexpected.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum LoopUnitError {
    #[error("invalid loop unit declaration `{name}`: {message}")]
    InvalidDeclaration { name: String, message: String },
    #[error("could not create loop unit directory `{}`: {source}", path.display())]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not write loop unit `{}`: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not inspect loop unit directory `{}`: {source}", path.display())]
    Inspect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not read loop unit `{}`: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub fn generate_loop_units(
    config: &LoopUnitGeneratorConfig,
    declarations: &[LoopUnitDeclaration],
) -> Result<Vec<LoopUnit>, LoopUnitError> {
    validate_config(config)?;
    let mut seen = BTreeSet::new();
    let mut units = Vec::with_capacity(declarations.len() * 2 + 2);
    if !declarations.is_empty() {
        units.push(LoopUnit {
            name: RECONCILER_SERVICE.to_owned(),
            contents: render_reconciler_service(config),
        });
        units.push(LoopUnit {
            name: RECONCILER_TIMER.to_owned(),
            contents: render_reconciler_timer(config),
        });
    }
    for declaration in declarations {
        validate_declaration(declaration)?;
        if !seen.insert(&declaration.name) {
            return Err(LoopUnitError::InvalidDeclaration {
                name: declaration.name.clone(),
                message: "name must be unique".to_owned(),
            });
        }
        units.push(LoopUnit {
            name: format!("{}{}.service", config.unit_prefix, declaration.name),
            contents: render_service(config, declaration),
        });
        units.push(LoopUnit {
            name: format!("{}{}.timer", config.unit_prefix, declaration.name),
            contents: render_timer(config, declaration),
        });
    }
    units.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(units)
}

/// Write generated unit files without enabling, starting, or reloading them.
pub fn render_loop_units(
    config: &LoopUnitGeneratorConfig,
    declarations: &[LoopUnitDeclaration],
    output: &Path,
) -> Result<Vec<PathBuf>, LoopUnitError> {
    let units = generate_loop_units(config, declarations)?;
    fs::create_dir_all(output).map_err(|source| LoopUnitError::CreateDirectory {
        path: output.to_path_buf(),
        source,
    })?;
    let mut written = Vec::with_capacity(units.len());
    for unit in units {
        let path = output.join(unit.name);
        fs::write(&path, unit.contents).map_err(|source| LoopUnitError::Write {
            path: path.clone(),
            source,
        })?;
        written.push(path);
    }
    Ok(written)
}

pub fn check_loop_units_drift(
    config: &LoopUnitGeneratorConfig,
    declarations: &[LoopUnitDeclaration],
    installed: &Path,
) -> Result<LoopUnitDrift, LoopUnitError> {
    let expected = generate_loop_units(config, declarations)?
        .into_iter()
        .map(|unit| (unit.name, unit.contents))
        .collect::<BTreeMap<_, _>>();
    let entries = fs::read_dir(installed).map_err(|source| LoopUnitError::Inspect {
        path: installed.to_path_buf(),
        source,
    })?;
    let mut actual_names = BTreeSet::new();
    let mut changed = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| LoopUnitError::Inspect {
            path: installed.to_path_buf(),
            source,
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !managed_unit_name(config, &name) {
            continue;
        }
        actual_names.insert(name.clone());
        if let Some(expected_contents) = expected.get(&name) {
            let path = entry.path();
            let actual =
                fs::read_to_string(&path).map_err(|source| LoopUnitError::Read { path, source })?;
            if &actual != expected_contents {
                changed.push(name);
            }
        }
    }
    let expected_names = expected.keys().cloned().collect::<BTreeSet<_>>();
    Ok(LoopUnitDrift {
        missing: expected_names.difference(&actual_names).cloned().collect(),
        changed,
        unexpected: actual_names.difference(&expected_names).cloned().collect(),
    })
}

/// Check the structural no-shell invariant of generated loop services.
#[must_use]
pub fn loop_execstart_is_not_shell(config: &LoopUnitGeneratorConfig, units: &[LoopUnit]) -> bool {
    units
        .iter()
        .filter(|unit| {
            unit.name.starts_with(&config.unit_prefix) && unit.name.ends_with(".service")
        })
        .all(|unit| {
            let starts = unit
                .contents
                .lines()
                .filter_map(|line| line.strip_prefix("ExecStart="))
                .collect::<Vec<_>>();
            starts.len() == 1
                && !starts[0].is_empty()
                && !starts[0].contains([';', '|', '&', '$', '`'])
                && !starts[0].contains("sh -c")
        })
}

fn validate_config(config: &LoopUnitGeneratorConfig) -> Result<(), LoopUnitError> {
    if config.unit_prefix.is_empty()
        || config
            .unit_prefix
            .bytes()
            .any(|byte| matches!(byte, b'/' | b'\0' | b'\n' | b'\r'))
    {
        return Err(LoopUnitError::InvalidDeclaration {
            name: config.unit_prefix.clone(),
            message: "unit prefix must be a safe non-empty filename prefix".to_owned(),
        });
    }
    if config.generated_header.contains('\0') {
        return Err(LoopUnitError::InvalidDeclaration {
            name: config.unit_prefix.clone(),
            message: "generated header must not contain NUL".to_owned(),
        });
    }
    Ok(())
}

fn validate_declaration(declaration: &LoopUnitDeclaration) -> Result<(), LoopUnitError> {
    let invalid_name = declaration.name.is_empty()
        || !declaration
            .name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if invalid_name {
        return invalid(declaration, "name must be a safe unit component");
    }
    if declaration.argv.is_empty()
        || declaration
            .argv
            .iter()
            .any(|argument| argument.is_empty() || argument.contains(['\0', '\n', '\r']))
    {
        return invalid(
            declaration,
            "argv must contain non-empty single-line values",
        );
    }
    if declaration.on_calendars.is_empty()
        || declaration
            .on_calendars
            .iter()
            .any(|calendar| calendar.is_empty() || calendar.contains(['\n', '\r']))
    {
        return invalid(
            declaration,
            "OnCalendar values must be non-empty single lines",
        );
    }
    if declaration
        .environment
        .iter()
        .any(|(name, value)| !valid_environment_name(name) || value.contains(['\n', '\r']))
    {
        return invalid(declaration, "environment contains an invalid name or value");
    }
    for name in [
        declaration.ceiling_environment.concurrent.as_deref(),
        declaration.ceiling_environment.spend_usd.as_deref(),
        declaration.ceiling_environment.tokens.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if !valid_environment_name(name) {
            return invalid(declaration, "ceiling environment name is invalid");
        }
    }
    if declaration
        .ceilings
        .spend_usd
        .is_some_and(|value| !value.is_finite())
    {
        return invalid(declaration, "spend ceiling must be finite");
    }
    Ok(())
}

fn invalid<T>(declaration: &LoopUnitDeclaration, message: &str) -> Result<T, LoopUnitError> {
    Err(LoopUnitError::InvalidDeclaration {
        name: declaration.name.clone(),
        message: message.to_owned(),
    })
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn managed_unit_name(config: &LoopUnitGeneratorConfig, name: &str) -> bool {
    name == RECONCILER_SERVICE
        || name == RECONCILER_TIMER
        || (name.starts_with(&config.unit_prefix)
            && (name.ends_with(".service") || name.ends_with(".timer")))
}

fn render_reconciler_service(config: &LoopUnitGeneratorConfig) -> String {
    format!(
        "{}[Unit]\nDescription=Reconcile Ostrom loops from the current policy version\nWants=network-online.target\nAfter=network-online.target\n\n[Service]\nType=oneshot\nExecStart=ostrom up\n\n[Install]\nWantedBy=default.target\n",
        config.generated_header
    )
}

fn render_reconciler_timer(config: &LoopUnitGeneratorConfig) -> String {
    format!(
        "{}[Unit]\nDescription=Periodically reconcile Ostrom loops from the current policy version\n\n[Timer]\nOnBootSec=1min\nOnUnitActiveSec=5min\nAccuracySec=1min\nUnit={RECONCILER_SERVICE}\n\n[Install]\nWantedBy=timers.target\n",
        config.generated_header
    )
}

fn render_service(config: &LoopUnitGeneratorConfig, declaration: &LoopUnitDeclaration) -> String {
    let mut source = String::new();
    source.push_str(&config.generated_header);
    source.push_str("[Unit]\n");
    source.push_str(&format!("Description=Ostrom loop {}\n", declaration.name));
    source.push_str("Wants=network-online.target\nAfter=network-online.target\n\n");
    source.push_str("[Service]\nType=oneshot\n");
    for (name, value) in &declaration.environment {
        source.push_str(&format!("Environment={name}={}\n", quote_argument(value)));
    }
    render_ceilings(
        &mut source,
        &declaration.ceiling_environment,
        declaration.ceilings,
    );
    source.push_str("ExecStart=");
    source.push_str(
        &declaration
            .argv
            .iter()
            .map(|argument| quote_argument(argument))
            .collect::<Vec<_>>()
            .join(" "),
    );
    source.push('\n');
    source.push_str("TimeoutStartSec=1800\nKillMode=control-group\n");
    source
}

fn render_ceilings(source: &mut String, names: &CeilingEnvironmentNames, ceilings: RunCeilings) {
    if let (Some(name), Some(value)) = (&names.spend_usd, ceilings.spend_usd) {
        source.push_str(&format!("Environment={name}={}\n", render_number(value)));
    }
    if let (Some(name), Some(value)) = (&names.concurrent, ceilings.concurrent) {
        source.push_str(&format!("Environment={name}={value}\n"));
    }
    if let (Some(name), Some(value)) = (&names.tokens, ceilings.tokens) {
        source.push_str(&format!("Environment={name}={value}\n"));
    }
}

fn render_timer(config: &LoopUnitGeneratorConfig, declaration: &LoopUnitDeclaration) -> String {
    let calendars = declaration
        .on_calendars
        .iter()
        .map(|calendar| format!("OnCalendar={calendar}\n"))
        .collect::<String>();
    format!(
        "{}[Unit]\nDescription=Ostrom loop {} schedule\n\n[Timer]\n{calendars}Persistent=true\nUnit={}{}.service\n\n[Install]\nWantedBy=timers.target\n",
        config.generated_header, declaration.name, config.unit_prefix, declaration.name
    )
}

fn quote_argument(value: &str) -> String {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"/._:@%+=,-".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn render_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use tempfile::tempdir;

    use super::*;

    fn generator_config() -> LoopUnitGeneratorConfig {
        LoopUnitGeneratorConfig {
            generated_header: "# Generated by `ostrom loops render`; do not edit.\n".to_owned(),
            unit_prefix: "ostrom-loop-".to_owned(),
        }
    }

    fn actor_environment_name() -> String {
        "OSTROM_ACTOR".to_owned()
    }

    fn concurrent_environment_name() -> String {
        "MANDATE_MAX_IMPLEMENTERS".to_owned()
    }

    fn spend_environment_name() -> String {
        "MANDATE_DAILY_CAP_USD".to_owned()
    }

    fn token_environment_name() -> String {
        "MANDATE_ORDER_TOKEN_CEILING".to_owned()
    }

    fn declarations() -> Vec<LoopUnitDeclaration> {
        vec![
            LoopUnitDeclaration {
                name: "builder-day".to_owned(),
                argv: vec![
                    "ostrom".to_owned(),
                    "loop".to_owned(),
                    "run".to_owned(),
                    "builder-day".to_owned(),
                ],
                on_calendars: vec!["*-*-* 08..21:15:00".to_owned()],
                environment: BTreeMap::from([(actor_environment_name(), "builder".to_owned())]),
                ceiling_environment: CeilingEnvironmentNames {
                    concurrent: Some(concurrent_environment_name()),
                    spend_usd: Some(spend_environment_name()),
                    tokens: Some(token_environment_name()),
                },
                ceilings: RunCeilings {
                    concurrent: Some(6),
                    spend_usd: Some(50.0),
                    tokens: Some(200_000),
                },
            },
            LoopUnitDeclaration {
                name: "builder-night".to_owned(),
                argv: vec![
                    "ostrom".to_owned(),
                    "loop".to_owned(),
                    "run".to_owned(),
                    "builder-night".to_owned(),
                ],
                on_calendars: vec!["*-*-* 23,02,05:15:00".to_owned()],
                environment: BTreeMap::from([(actor_environment_name(), "builder".to_owned())]),
                ceiling_environment: CeilingEnvironmentNames {
                    concurrent: Some(concurrent_environment_name()),
                    spend_usd: Some(spend_environment_name()),
                    tokens: Some(token_environment_name()),
                },
                ceilings: RunCeilings {
                    concurrent: Some(2),
                    spend_usd: Some(50.0),
                    tokens: Some(200_000),
                },
            },
        ]
    }

    #[test]
    fn renders_exact_cadence_lines_and_never_a_shell_execstart() {
        let config = generator_config();
        let units = generate_loop_units(&config, &declarations()).expect("units");
        let day = &units
            .iter()
            .find(|unit| unit.name == format!("{}builder-day.timer", config.unit_prefix))
            .expect("day timer")
            .contents;
        let night = &units
            .iter()
            .find(|unit| unit.name == format!("{}builder-night.timer", config.unit_prefix))
            .expect("night timer")
            .contents;
        assert!(day.contains("OnCalendar=*-*-* 08..21:15:00\n"));
        assert!(night.contains("OnCalendar=*-*-* 23,02,05:15:00\n"));
        assert!(loop_execstart_is_not_shell(&config, &units));
        let service = &units
            .iter()
            .find(|unit| unit.name == format!("{}builder-day.service", config.unit_prefix))
            .expect("day service")
            .contents;
        assert!(service.contains("ExecStart=ostrom loop run builder-day\n"));
        assert!(service.contains(&format!("Environment={}=50\n", spend_environment_name())));
        assert!(!service.contains("systemctl"));
        let reconciler = units
            .iter()
            .find(|unit| unit.name == RECONCILER_SERVICE)
            .expect("reconciler boot unit");
        assert!(reconciler.contents.contains("ExecStart=ostrom up\n"));
        assert!(!reconciler.contents.contains("systemctl"));
        let timer = units
            .iter()
            .find(|unit| unit.name == RECONCILER_TIMER)
            .expect("reconciler timer");
        assert!(timer.contents.contains("OnUnitActiveSec=5min\n"));
        assert!(timer.contents.contains("Unit=ostrom-up.service\n"));
        assert!(!timer.contents.contains("systemctl"));
    }

    #[test]
    fn drift_names_missing_changed_and_unexpected_managed_units() {
        let config = generator_config();
        let declarations = declarations();
        let root = tempdir().expect("fixture");
        let written = render_loop_units(&config, &declarations, root.path()).expect("render");
        fs::write(&written[0], "hand edit\n").expect("edit fixture");
        fs::remove_file(&written[1]).expect("remove fixture");
        let stale = format!("{}stale.timer", config.unit_prefix);
        fs::write(root.path().join(&stale), "stale\n").expect("write stale fixture");
        fs::write(root.path().join("unrelated.service"), "unmanaged\n")
            .expect("write unmanaged fixture");

        let drift =
            check_loop_units_drift(&config, &declarations, root.path()).expect("drift check");
        assert_eq!(
            drift.changed,
            [format!("{}builder-day.service", config.unit_prefix)]
        );
        assert_eq!(
            drift.missing,
            [format!("{}builder-day.timer", config.unit_prefix)]
        );
        assert_eq!(drift.unexpected, [stale]);
    }

    #[test]
    fn caller_supplied_legacy_naming_preserves_rendered_bytes() {
        let config = generator_config();
        let units = generate_loop_units(&config, &declarations()).expect("units");
        let service = units
            .iter()
            .find(|unit| unit.name == format!("{}builder-day.service", config.unit_prefix))
            .expect("day service");
        let expected = concat!(
            "# Generated by `ostrom loops",
            " render`; do not edit.\n",
            "[Unit]\n",
            "Description=Ostrom loop builder-day\n",
            "Wants=network-online.target\n",
            "After=network-online.target\n\n",
            "[Service]\n",
            "Type=oneshot\n",
            "Environment=OSTROM",
            "_ACTOR=builder\n",
            "Environment=MANDATE",
            "_DAILY_CAP_USD=50\n",
            "Environment=MANDATE",
            "_MAX_IMPLEMENTERS=6\n",
            "Environment=MANDATE",
            "_ORDER_TOKEN_CEILING=200000\n",
            "ExecStart=ostrom loop run builder-day\n",
            "TimeoutStartSec=1800\n",
            "KillMode=control-group\n",
        );

        assert_eq!(service.contents, expected);
    }
}
