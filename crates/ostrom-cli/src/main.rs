use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use directories::BaseDirs;
use ostrom_checks::{ActionFault, ActionRegistry};
use ostrom_core::{
    Catalogue, CatalogueEnumeration, CheckContractError, CheckDocument, CheckFault, RepositoryName,
    ResolvedCheck,
};
use ostrom_store::{
    AuditOptions, DispatchOutcome, DispatchRequest, ExecutableAssessmentDeriver, GateError,
    GateOptions, MigrationOutcome, OstromPaths, PlanOptions, PublishTarget, SelectAction,
    SelectError, SelectOutcome, SelectRequest, SweepError, SweepMode, SweepOptions,
    SweepParityOptions, UnavailableAssessmentDeriver, acquire_org_from_github, audit,
    encode_org_snapshots, encode_selection, grant_excuse, list_excuses, list_queue_json,
    local_drift, migrate, run_dispatch, run_gate, run_plan, run_selection, run_sweep,
    run_sweep_parity,
};

#[derive(Debug, Parser)]
#[command(name = "ostrom", version, about = "Ostrom workflow commons CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Turn a durable work order into a running implementer.
    Dispatch {
        #[arg(allow_hyphen_values = true)]
        arguments: Vec<String>,
    },
    /// Select graph-dispatchable work using the settled mandate precedence.
    SelectWork {
        #[arg(allow_hyphen_values = true)]
        arguments: Vec<String>,
    },
    /// Evaluate one pull request against the artifact merge gate.
    Gate {
        #[arg(num_args = 0.., allow_hyphen_values = true)]
        target: Vec<String>,
    },
    /// Inspect the private queue.
    Queue {
        #[command(subcommand)]
        command: QueueCommand,
    },
    /// Move legacy Claude-hosted data to XDG config and state roots.
    Migrate,
    /// Compare native and legacy command output in isolated scratch homes.
    Parity {
        #[command(subcommand)]
        command: ParityCommand,
    },
    /// Reconcile the governed GitHub roster into the private queue.
    Sweep {
        /// Force full/incremental acquisition or select automatically.
        #[arg(long, value_enum, default_value_t = CliSweepMode::Auto)]
        mode: CliSweepMode,
        /// Recorded GitHub responses for a hermetic parity run.
        #[arg(long, hide = true)]
        fixture: Option<PathBuf>,
        /// Explicit publication destination. Omission means no publication.
        #[arg(long)]
        publish_repository: Option<String>,
        /// Internal organization worker, run beneath a native minted token.
        #[arg(long, hide = true)]
        inner_org: Option<String>,
        /// One clock shared by every organization worker.
        #[arg(long, hide = true)]
        started_at: Option<String>,
    },
    /// Reconcile the portfolio, assess authored goals, and write plan.json.
    Plan {
        /// Force full/incremental acquisition or select automatically.
        #[arg(long, value_enum, default_value_t = CliSweepMode::Auto)]
        mode: CliSweepMode,
        /// Recorded GitHub responses for a hermetic parity run.
        #[arg(long, hide = true)]
        fixture: Option<PathBuf>,
        /// One clock shared by the sweep and goal evaluation.
        #[arg(long, hide = true)]
        started_at: Option<String>,
    },
    /// Audit merged pull requests against verdicts recorded at their merged SHA.
    Audit {
        /// Number of days in the merged-at window.
        #[arg(long, default_value_t = 30)]
        days: u64,
    },
    /// Grant or inspect SHA-scoped merge-gate exceptions.
    Excuse {
        #[command(subcommand)]
        command: ExcuseCommand,
    },
    /// Scan local Git repositories for drift without changing them.
    LocalDrift {
        /// Suppress network-backed pull-request classification.
        #[arg(long)]
        local_only: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ExcuseCommand {
    /// Grant one condition exception at the pull request's current head SHA.
    Grant {
        target: String,
        condition: String,
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
        reason: Vec<String>,
    },
    /// List exception events, optionally for exactly one pull request.
    List { target: Option<String> },
}

#[derive(Debug, Subcommand)]
enum QueueCommand {
    /// List pending and deferred queue entries.
    List {
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
    },
}

#[derive(Debug, Subcommand)]
enum ParityCommand {
    /// Compare native and legacy sweep rows by id and field.
    Sweep {
        /// One clock shared by both implementations.
        #[arg(long)]
        started_at: Option<String>,
        /// Recorded GitHub responses for a hermetic native-side test.
        #[arg(long, hide = true)]
        fixture: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliSweepMode {
    Auto,
    Full,
    Incremental,
}

impl From<CliSweepMode> for SweepMode {
    fn from(value: CliSweepMode) -> Self {
        match value {
            CliSweepMode::Auto => Self::Auto,
            CliSweepMode::Full => Self::Full,
            CliSweepMode::Incremental => Self::Incremental,
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let paths = OstromPaths::resolve()?;
    match cli.command {
        Command::Dispatch { arguments } => {
            run_dispatch_command(arguments);
        }
        Command::SelectWork { arguments } => {
            run_select_work(arguments);
        }
        Command::Gate { target } => {
            if target.len() != 1 {
                let error = GateError::InvalidTarget;
                eprintln!("{error}");
                std::process::exit(error.exit_code());
            }
            let timestamp = env::var("MANDATE_GATE_TIME")
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| {
                    DateTime::<Utc>::from(SystemTime::now())
                        .format("%Y-%m-%dT%H:%M:%SZ")
                        .to_string()
                });
            let output = match run_gate(&GateOptions {
                paths,
                working_directory: env::current_dir()?,
                target: target[0].clone(),
                timestamp,
            }) {
                Ok(output) => output,
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(error.exit_code());
                }
            };
            io::stdout().write_all(output.stdout.as_bytes())?;
            io::stderr().write_all(output.stderr.as_bytes())?;
            if output.exit_code != 0 {
                std::process::exit(output.exit_code);
            }
        }
        Command::Queue {
            command: QueueCommand::List { format },
        } => match format {
            OutputFormat::Json => {
                io::stdout().write_all(&list_queue_json(&paths.queue_file())?)?;
            }
        },
        Command::Migrate => {
            let legacy = legacy_home()?;
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            match migrate(&legacy, &paths, now)? {
                MigrationOutcome::Migrated => println!(
                    "migrated Ostrom config to {} and state to {}; legacy pointer retained at {}",
                    paths.config.display(),
                    paths.state.display(),
                    legacy.display()
                ),
                MigrationOutcome::AlreadyMigrated => {
                    println!("Ostrom state is already migrated; legacy pointer is unchanged")
                }
                MigrationOutcome::NothingToMigrate => {
                    println!("no legacy Ostrom state exists; nothing to migrate")
                }
            }
        }
        Command::Parity {
            command:
                ParityCommand::Sweep {
                    started_at,
                    fixture,
                },
        } => {
            let started_at = resolve_started_at(started_at.as_deref())?;
            let cwd = env::current_dir()?;
            let executable = env::current_exe()?;
            let plugin_root = env::var_os("OSTROM_PLUGIN_ROOT")
                .or_else(|| env::var_os("CLAUDE_PLUGIN_ROOT"))
                .map_or_else(|| cwd.join("plugins/ostrom"), PathBuf::from);
            let options = SweepParityOptions::from_environment(
                cwd,
                executable,
                plugin_root,
                started_at,
                fixture,
            )?;
            let outcome = run_sweep_parity(&options)?;
            if outcome.differences.is_empty() {
                println!(
                    "parity sweep: zero divergences across {} row(s)",
                    outcome.row_count
                );
            } else {
                for (field, ids) in &outcome.differences {
                    println!(
                        "parity sweep: {field} differs on {} row(s): {}",
                        ids.len(),
                        ids.join(", ")
                    );
                }
                std::process::exit(1);
            }
        }
        Command::Sweep {
            mode,
            fixture,
            publish_repository,
            inner_org,
            started_at,
        } => {
            let started_at = resolve_started_at(started_at.as_deref())?;
            if let Some(org) = inner_org {
                let cwd = env::current_dir()?;
                let snapshots =
                    match acquire_org_from_github(&paths, &cwd, &org, started_at, mode.into()) {
                        Ok(snapshots) => snapshots,
                        Err(error @ SweepError::BranchListingTruncated(_)) => {
                            eprintln!("{error}");
                            std::process::exit(6);
                        }
                        Err(error) => return Err(error.into()),
                    };
                io::stdout().write_all(&encode_org_snapshots(snapshots)?)?;
                return Ok(());
            }
            let publish = publish_repository.map_or(Ok(PublishTarget::Disabled), |repository| {
                RepositoryName::new(repository).map(PublishTarget::Repository)
            })?;
            let cwd = env::current_dir()?;
            let executable = env::current_exe()?;
            let plugin_root = env::var_os("OSTROM_PLUGIN_ROOT")
                .or_else(|| env::var_os("CLAUDE_PLUGIN_ROOT"))
                .map_or_else(|| cwd.join("plugins/ostrom"), PathBuf::from);
            let outcome = run_sweep(&SweepOptions {
                paths,
                working_directory: cwd,
                executable,
                plugin_root,
                started_at,
                requested_mode: mode.into(),
                fixture,
                publish,
            })?;
            println!(
                "mandate sweep: {} projects; {} queue changes",
                outcome.project_count, outcome.queue_changes
            );
            for fault in &outcome.faults {
                eprintln!("mandate sweep: {fault}");
            }
        }
        Command::Plan {
            mode,
            fixture,
            started_at,
        } => {
            let started_at = resolve_started_at(started_at.as_deref())?;
            let cwd = env::current_dir()?;
            let executable = env::current_exe()?;
            let plugin_root = env::var_os("OSTROM_PLUGIN_ROOT")
                .or_else(|| env::var_os("CLAUDE_PLUGIN_ROOT"))
                .map_or_else(|| cwd.join("plugins/ostrom"), PathBuf::from);
            let check_resolutions = resolve_plan_checks(&paths, &cwd, &plugin_root)?;
            let options = PlanOptions {
                sweep: SweepOptions {
                    paths: paths.clone(),
                    working_directory: cwd,
                    executable,
                    plugin_root,
                    started_at,
                    requested_mode: mode.into(),
                    fixture,
                    publish: PublishTarget::Disabled,
                },
                resolved_checks: check_resolutions.resolved,
                check_resolution_faults: check_resolutions.faults,
                catalogue_fault: check_resolutions.catalogue_fault,
            };
            let mut deriver: Box<dyn ostrom_store::AssessmentDeriver> =
                if let Some(executable) = env::var_os("OSTROM_PLAN_DERIVER") {
                    Box::new(ExecutableAssessmentDeriver::new(PathBuf::from(executable)))
                } else {
                    Box::new(UnavailableAssessmentDeriver)
                };
            let plan = run_plan(&options, deriver.as_mut())?;
            println!(
                "ostrom plan: {} goals; {} ranked items; {} faults; wrote {}",
                plan.goals.len(),
                plan.ranking.ordered.len(),
                plan.faults.len(),
                paths.state.join("plan.json").display()
            );
        }
        Command::Audit { days } => {
            let audit_time = environment_time("MANDATE_AUDIT_TIME").unwrap_or_else(|message| {
                eprintln!("mandate audit: {message}");
                std::process::exit(2);
            });
            let working_directory = env::current_dir()?;
            match audit(&AuditOptions {
                paths,
                working_directory,
                days,
                audit_time,
            }) {
                Ok(output) => io::stdout().write_all(output.as_bytes())?,
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(error.exit_code());
                }
            }
        }
        Command::Excuse { command } => match command {
            ExcuseCommand::Grant {
                target,
                condition,
                reason,
            } => {
                let timestamp =
                    optional_environment_time("MANDATE_EXCUSE_TIME").unwrap_or_else(|message| {
                        eprintln!("mandate excuse: {message}");
                        std::process::exit(3);
                    });
                match grant_excuse(&paths, &target, &condition, &reason, timestamp) {
                    Ok(output) => io::stdout().write_all(output.as_bytes())?,
                    Err(error) => {
                        eprintln!("{error}");
                        std::process::exit(error.exit_code());
                    }
                }
            }
            ExcuseCommand::List { target } => match list_excuses(&paths, target.as_deref()) {
                Ok(output) => io::stdout().write_all(output.as_bytes())?,
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(error.exit_code());
                }
            },
        },
        Command::LocalDrift { local_only } => {
            let working_directory = env::current_dir()?;
            match local_drift(&paths, &working_directory, local_only) {
                Ok(output) => io::stdout().write_all(output.as_bytes())?,
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(error.exit_code());
                }
            }
        }
    }
    Ok(())
}

fn environment_time(name: &str) -> Result<DateTime<Utc>, String> {
    optional_environment_time(name)
        .map(|value| value.unwrap_or_else(|| DateTime::<Utc>::from(SystemTime::now())))
}

fn optional_environment_time(name: &str) -> Result<Option<DateTime<Utc>>, String> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| {
            DateTime::parse_from_rfc3339(&value)
                .map(|parsed| parsed.with_timezone(&Utc))
                .map_err(|_| format!("{name} must be an RFC 3339 timestamp"))
        })
        .transpose()
}

fn run_dispatch_command(arguments: Vec<String>) -> ! {
    let [order_file] = arguments.as_slice() else {
        eprintln!("usage: dispatch.sh <work-order-file>");
        std::process::exit(2);
    };
    let working_directory = env::current_dir().unwrap_or_else(|error| {
        eprintln!("ostrom dispatch: could not resolve working directory: {error}");
        std::process::exit(1);
    });
    let plugin_root = env::var_os("OSTROM_PLUGIN_ROOT")
        .or_else(|| env::var_os("CLAUDE_PLUGIN_ROOT"))
        .map_or_else(|| working_directory.join("plugins/ostrom"), PathBuf::from);
    let request = DispatchRequest {
        paths: compatible_command_paths(),
        working_directory,
        plugin_root,
        order_file: PathBuf::from(order_file),
    };
    match run_dispatch(&request) {
        Ok(DispatchOutcome::Started(unit)) => {
            println!("{unit}");
            std::process::exit(0);
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(error.code);
        }
    }
}

fn run_select_work(arguments: Vec<String>) -> ! {
    let usage = || {
        eprintln!("usage: select-work.sh list | select <owner> [already-attempted-id ...]");
        std::process::exit(2);
    };
    let action = match arguments.as_slice() {
        [action] if action == "list" => SelectAction::List,
        [action, owner, attempted @ ..] if action == "select" && !owner.is_empty() => {
            SelectAction::Select {
                owner: owner.clone(),
                attempted: attempted.iter().cloned().collect(),
            }
        }
        [action, owner, ..] if action == "select" && owner.is_empty() => {
            eprintln!("mandate selection: owner must not be empty");
            std::process::exit(2);
        }
        _ => usage(),
    };
    let paths = compatible_command_paths();
    let request = SelectRequest {
        paths,
        working_directory: env::current_dir().unwrap_or_else(|error| {
            eprintln!("mandate selection: could not resolve working directory: {error}");
            std::process::exit(1);
        }),
        action,
    };
    match run_selection(&request) {
        Ok((outcome, diagnostics)) => {
            for diagnostic in diagnostics {
                eprintln!("{diagnostic}");
            }
            if outcome == SelectOutcome::Empty {
                let code = if matches!(request.action, SelectAction::Select { .. }) {
                    3
                } else {
                    0
                };
                std::process::exit(code);
            }
            if let Err(error) = io::stdout().write_all(&encode_selection(&outcome)) {
                eprintln!("mandate selection: could not write selection: {error}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        Err(error) => {
            eprintln!("{error}");
            let code = match error {
                SelectError::MissingState
                | SelectError::StateRead { .. }
                | SelectError::InvalidGraph
                | SelectError::RankingMismatch
                | SelectError::StaleRanking(_) => 4,
                _ => 1,
            };
            std::process::exit(code);
        }
    }
}

fn compatible_command_paths() -> OstromPaths {
    if env::var_os("OSTROM_HOME").is_some_and(|home| !home.to_string_lossy().trim().is_empty()) {
        return OstromPaths::resolve().unwrap_or_else(|error| {
            eprintln!("ostrom: {error}");
            std::process::exit(1);
        });
    }
    if let Some(config) = env::var_os("CLAUDE_CONFIG_DIR") {
        let root = PathBuf::from(config).join("ostrom");
        return OstromPaths {
            config: root.clone(),
            state: root,
        };
    }
    OstromPaths::resolve().unwrap_or_else(|error| {
        eprintln!("ostrom: {error}");
        std::process::exit(1);
    })
}

struct PlanCheckResolutions {
    resolved: BTreeMap<String, ResolvedCheck>,
    faults: BTreeMap<String, CheckFault>,
    catalogue_fault: Option<CheckFault>,
}

fn resolve_plan_checks(
    paths: &OstromPaths,
    cwd: &Path,
    plugin_root: &Path,
) -> Result<PlanCheckResolutions, ActionFault> {
    let mut enumeration = CatalogueEnumeration {
        catalogues: Vec::new(),
        complete: true,
    };
    let mut catalogue_fault = None;
    let sources = BTreeSet::from([
        paths.config.join("checks.yaml"),
        cwd.join(".ostrom/checks.yaml"),
    ]);
    for source in sources {
        match fs::read_to_string(&source) {
            Ok(text) => match CheckDocument::from_yaml(&text) {
                Ok(document) => enumeration.catalogues.push(Catalogue { document }),
                Err(error) => {
                    enumeration.complete = false;
                    catalogue_fault.get_or_insert_with(|| contract_fault(&error));
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {
                enumeration.complete = false;
                catalogue_fault.get_or_insert_with(truncated_catalogue_fault);
            }
        }
    }

    if catalogue_fault.is_some() {
        return Ok(PlanCheckResolutions {
            resolved: BTreeMap::new(),
            faults: BTreeMap::new(),
            catalogue_fault,
        });
    }

    let ids = enumeration
        .catalogues
        .iter()
        .flat_map(|catalogue| catalogue.document.checks.keys().cloned())
        .collect::<BTreeSet<_>>();
    let registry = ActionRegistry::core(plugin_root.join("dist/doctor.js"))?;
    let mut resolved = BTreeMap::new();
    let mut faults = BTreeMap::new();
    for id in ids {
        match registry.prepare(&id, &enumeration) {
            Ok(prepared) => {
                resolved.insert(id, prepared.resolved().clone());
            }
            Err(error) => {
                faults.insert(id, action_fault(&error));
            }
        }
    }
    Ok(PlanCheckResolutions {
        resolved,
        faults,
        catalogue_fault,
    })
}

fn contract_fault(error: &CheckContractError) -> CheckFault {
    CheckFault {
        name: error
            .fault_name()
            .unwrap_or("invalid_check_definition")
            .to_owned(),
        detail: None,
    }
}

fn truncated_catalogue_fault() -> CheckFault {
    CheckFault {
        name: "check_catalog_truncated".to_owned(),
        detail: None,
    }
}

fn action_fault(error: &ActionFault) -> CheckFault {
    CheckFault {
        name: error.name().to_owned(),
        detail: error.detail().map(str::to_owned),
    }
}

fn resolve_started_at(value: Option<&str>) -> Result<DateTime<Utc>, io::Error> {
    if let Some(value) = value {
        return parse_started_at(value, "--started-at");
    }
    match env::var("MANDATE_SWEEP_TIME") {
        // The shell reads this as `${MANDATE_SWEEP_TIME:-<now>}`, so an empty
        // value means absent rather than malformed. A non-empty value that
        // will not parse is an error: a parity harness that silently un-pins
        // its clock produces a confident wrong answer.
        Ok(value) if value.is_empty() => Ok(DateTime::<Utc>::from(SystemTime::now())),
        Ok(value) => parse_started_at(&value, "MANDATE_SWEEP_TIME"),
        Err(env::VarError::NotPresent) => Ok(DateTime::<Utc>::from(SystemTime::now())),
        Err(env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MANDATE_SWEEP_TIME must be a valid Unicode RFC3339 instant",
        )),
    }
}

fn parse_started_at(value: &str, source: &str) -> Result<DateTime<Utc>, io::Error> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{source} is not a valid RFC3339 instant: {error}"),
            )
        })
}

fn legacy_home() -> Result<PathBuf, Box<dyn std::error::Error>> {
    // The override exists for hermetic operator rehearsals. Tests call the
    // migration library with explicit temporary paths and never resolve this
    // default, which is the live directory the task forbids the suite to read.
    if let Some(path) = env::var_os("OSTROM_LEGACY_HOME") {
        return Ok(PathBuf::from(path));
    }
    let base = BaseDirs::new().ok_or("could not resolve the legacy home directory")?;
    Ok(base.home_dir().join(".claude/ostrom"))
}
