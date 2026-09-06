use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::RunCeilings;

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
    /// Names Umwelt must decline to have an opinion about. It never generates
    /// or compares their contents and cannot know whether their absence is
    /// drift, so they are reported neither missing nor unexpected.
    pub externally_managed: Vec<String>,
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
    pub description: String,
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
    let mut units = Vec::with_capacity(declarations.len() * 2);
    for declaration in declarations {
        validate_declaration(declaration)?;
        if !seen.insert(&declaration.name) {
            return Err(LoopUnitError::InvalidDeclaration {
                name: declaration.name.clone(),
                message: "name must be unique".to_owned(),
            });
        }
        let service_name = format!("{}{}.service", config.unit_prefix, declaration.name);
        let timer_name = format!("{}{}.timer", config.unit_prefix, declaration.name);
        if externally_managed_name(config, &service_name)
            || externally_managed_name(config, &timer_name)
        {
            return Err(LoopUnitError::InvalidDeclaration {
                name: declaration.name.clone(),
                message: "generated names must not also be externally managed".to_owned(),
            });
        }
        units.push(LoopUnit {
            name: service_name,
            contents: render_service(config, declaration),
        });
        units.push(LoopUnit {
            name: timer_name,
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
        if externally_managed_name(config, &name) {
            continue;
        }
        if !name.starts_with(&config.unit_prefix)
            || !(name.ends_with(".service") || name.ends_with(".timer"))
        {
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
    let expected_names = expected
        .keys()
        .filter(|name| !externally_managed_name(config, name))
        .cloned()
        .collect::<BTreeSet<_>>();
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
    if config.externally_managed.iter().any(|name| {
        name.is_empty()
            || name.contains(['/', '\0', '\n', '\r'])
            || !(name.ends_with(".service") || name.ends_with(".timer"))
    }) {
        return Err(LoopUnitError::InvalidDeclaration {
            name: config.unit_prefix.clone(),
            message: "externally managed names must be safe service or timer filenames".to_owned(),
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
    if declaration.description.is_empty() || declaration.description.contains(['\n', '\r']) {
        return invalid(declaration, "description must be a non-empty single line");
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

fn externally_managed_name(config: &LoopUnitGeneratorConfig, name: &str) -> bool {
    config
        .externally_managed
        .iter()
        .any(|external| external == name)
}

fn render_service(config: &LoopUnitGeneratorConfig, declaration: &LoopUnitDeclaration) -> String {
    let mut source = String::new();
    source.push_str(&config.generated_header);
    source.push_str("[Unit]\n");
    source.push_str(&format!("Description={}\n", declaration.description));
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
        "{}[Unit]\nDescription={} schedule\n\n[Timer]\n{calendars}Persistent=true\nUnit={}{}.service\n\n[Install]\nWantedBy=timers.target\n",
        config.generated_header, declaration.description, config.unit_prefix, declaration.name
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
            generated_header: "# Generated by `example render`; do not edit.\n".to_owned(),
            unit_prefix: "example-loop-".to_owned(),
            externally_managed: Vec::new(),
        }
    }

    fn declarations() -> Vec<LoopUnitDeclaration> {
        vec![
            LoopUnitDeclaration {
                name: "daytime".to_owned(),
                description: "Example loop daytime".to_owned(),
                argv: vec![
                    "example-tool".to_owned(),
                    "run".to_owned(),
                    "daytime".to_owned(),
                ],
                on_calendars: vec!["*-*-* 08..21:15:00".to_owned()],
                environment: BTreeMap::from([("EXAMPLE_ACTOR".to_owned(), "builder".to_owned())]),
                ceiling_environment: CeilingEnvironmentNames {
                    concurrent: Some("EXAMPLE_MAX_WORKERS".to_owned()),
                    spend_usd: Some("EXAMPLE_DAILY_CAP_USD".to_owned()),
                    tokens: Some("EXAMPLE_TOKEN_CEILING".to_owned()),
                },
                ceilings: RunCeilings {
                    concurrent: Some(6),
                    spend_usd: Some(50.0),
                    tokens: Some(200_000),
                },
            },
            LoopUnitDeclaration {
                name: "nightly".to_owned(),
                description: "Example loop nightly".to_owned(),
                argv: vec![
                    "example-tool".to_owned(),
                    "run".to_owned(),
                    "nightly".to_owned(),
                ],
                on_calendars: vec!["*-*-* 23,02,05:15:00".to_owned()],
                environment: BTreeMap::from([("EXAMPLE_ACTOR".to_owned(), "builder".to_owned())]),
                ceiling_environment: CeilingEnvironmentNames {
                    concurrent: Some("EXAMPLE_MAX_WORKERS".to_owned()),
                    spend_usd: Some("EXAMPLE_DAILY_CAP_USD".to_owned()),
                    tokens: Some("EXAMPLE_TOKEN_CEILING".to_owned()),
                },
                ceilings: RunCeilings {
                    concurrent: Some(2),
                    spend_usd: Some(50.0),
                    tokens: Some(200_000),
                },
            },
        ]
    }

    fn externally_managed_config() -> LoopUnitGeneratorConfig {
        let mut config = generator_config();
        config.externally_managed = vec![
            "example-reconcile.service".to_owned(),
            "example-reconcile.timer".to_owned(),
        ];
        config
    }

    #[test]
    fn a_generated_name_declared_externally_managed_is_refused() {
        // The two meanings contradict: umwelt generates these contents and must
        // compare them, and has also been told to have no opinion about them.
        // Resolving that silently would let umwelt go quiet about a real change
        // to a unit it really generates, so the collision is refused instead.
        let mut config = generator_config();
        let declarations = declarations();
        config.externally_managed = vec![format!(
            "{}{}.service",
            config.unit_prefix, declarations[0].name
        )];

        let error = generate_loop_units(&config, &declarations)
            .expect_err("a generated name that is also externally managed must be refused");

        assert!(
            matches!(
                error,
                LoopUnitError::InvalidDeclaration { ref message, .. }
                    if message.contains("externally managed")
            ),
            "expected a loud refusal naming the collision, got {error:?}"
        );
    }

    #[test]
    fn a_generated_timer_name_declared_externally_managed_is_refused() {
        let mut config = generator_config();
        let declarations = declarations();
        config.externally_managed = vec![format!(
            "{}{}.timer",
            config.unit_prefix, declarations[0].name
        )];

        generate_loop_units(&config, &declarations)
            .expect_err("the timer half of the collision must be refused too");
    }

    #[test]
    fn renders_declared_units_from_caller_supplied_values_without_a_shell() {
        let config = generator_config();
        let units = generate_loop_units(&config, &declarations()).expect("units");

        assert_eq!(units.len(), 4, "one service and timer per declaration");
        assert!(
            units
                .iter()
                .all(|unit| unit.name.starts_with("example-loop-"))
        );
        assert!(units.iter().all(|unit| {
            unit.contents
                .starts_with("# Generated by `example render`; do not edit.\n")
        }));

        let daytime_service = &units
            .iter()
            .find(|unit| unit.name == "example-loop-daytime.service")
            .expect("daytime service")
            .contents;
        assert!(daytime_service.contains("Description=Example loop daytime\n"));
        assert!(daytime_service.contains("ExecStart=example-tool run daytime\n"));
        assert!(daytime_service.contains("Environment=EXAMPLE_ACTOR=builder\n"));
        assert!(daytime_service.contains("Environment=EXAMPLE_DAILY_CAP_USD=50\n"));
        assert!(daytime_service.contains("Environment=EXAMPLE_MAX_WORKERS=6\n"));
        assert!(daytime_service.contains("Environment=EXAMPLE_TOKEN_CEILING=200000\n"));

        let daytime_timer = &units
            .iter()
            .find(|unit| unit.name == "example-loop-daytime.timer")
            .expect("daytime timer")
            .contents;
        assert!(daytime_timer.contains("Description=Example loop daytime schedule\n"));
        assert!(daytime_timer.contains("OnCalendar=*-*-* 08..21:15:00\n"));
        assert!(daytime_timer.contains("Unit=example-loop-daytime.service\n"));

        let nightly_timer = &units
            .iter()
            .find(|unit| unit.name == "example-loop-nightly.timer")
            .expect("nightly timer")
            .contents;
        assert!(nightly_timer.contains("OnCalendar=*-*-* 23,02,05:15:00\n"));
        assert!(loop_execstart_is_not_shell(&config, &units));
    }

    #[test]
    fn drift_names_generated_missing_changed_and_unexpected_units() {
        let config = generator_config();
        let declarations = declarations();
        let root = tempdir().expect("fixture");
        let written = render_loop_units(&config, &declarations, root.path()).expect("render");
        fs::write(&written[0], "hand edit\n").expect("edit fixture");
        fs::remove_file(&written[1]).expect("remove fixture");
        fs::write(root.path().join("example-loop-stale.timer"), "stale\n")
            .expect("write stale fixture");
        fs::write(root.path().join("unrelated.service"), "unmanaged\n")
            .expect("write unmanaged fixture");

        let drift =
            check_loop_units_drift(&config, &declarations, root.path()).expect("drift check");
        assert_eq!(drift.changed, ["example-loop-daytime.service"]);
        assert_eq!(drift.missing, ["example-loop-daytime.timer"]);
        assert_eq!(drift.unexpected, ["example-loop-stale.timer"]);
    }

    #[test]
    fn absent_externally_managed_name_is_not_missing() {
        let config = externally_managed_config();
        let declarations = declarations();
        let root = tempdir().expect("fixture");
        render_loop_units(&config, &declarations, root.path()).expect("render loops");

        let drift =
            check_loop_units_drift(&config, &declarations, root.path()).expect("drift check");

        assert!(
            !drift
                .missing
                .contains(&"example-reconcile.service".to_owned())
        );
    }

    #[test]
    fn present_externally_managed_name_is_not_unexpected() {
        let config = externally_managed_config();
        let declarations = declarations();
        let root = tempdir().expect("fixture");
        render_loop_units(&config, &declarations, root.path()).expect("render loops");
        fs::write(
            root.path().join("example-reconcile.service"),
            "caller owned\n",
        )
        .expect("write external service");

        let drift =
            check_loop_units_drift(&config, &declarations, root.path()).expect("drift check");

        assert!(
            !drift
                .unexpected
                .contains(&"example-reconcile.service".to_owned())
        );
    }

    #[test]
    fn externally_managed_contents_are_never_compared() {
        let config = externally_managed_config();
        let declarations = declarations();
        let root = tempdir().expect("fixture");
        render_loop_units(&config, &declarations, root.path()).expect("render loops");
        fs::write(
            root.path().join("example-reconcile.service"),
            "arbitrary external contents\n",
        )
        .expect("write external service");

        let drift =
            check_loop_units_drift(&config, &declarations, root.path()).expect("drift check");

        assert!(
            !drift
                .changed
                .contains(&"example-reconcile.service".to_owned())
        );
    }
}
