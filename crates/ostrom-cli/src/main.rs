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
    ExecutableAssessmentDeriver, MigrationOutcome, OstromPaths, PlanOptions, PublishTarget,
    SweepMode, SweepOptions, UnavailableAssessmentDeriver, acquire_org_from_github,
    encode_org_snapshots, list_queue_json, migrate, run_plan, run_sweep,
};

#[derive(Debug, Parser)]
#[command(name = "ostrom", version, about = "Ostrom workflow commons CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect the private queue.
    Queue {
        #[command(subcommand)]
        command: QueueCommand,
    },
    /// Move legacy Claude-hosted data to XDG config and state roots.
    Migrate,
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
        /// Internal organization worker, always run beneath gh-as.sh.
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
}

#[derive(Debug, Subcommand)]
enum QueueCommand {
    /// List pending and deferred queue entries.
    List {
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let paths = OstromPaths::resolve()?;
    match cli.command {
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
        Command::Sweep {
            mode,
            fixture,
            publish_repository,
            inner_org,
            started_at,
        } => {
            let started_at = started_at
                .as_deref()
                .map(DateTime::parse_from_rfc3339)
                .transpose()?
                .map_or_else(
                    || DateTime::<Utc>::from(SystemTime::now()),
                    |value| value.with_timezone(&Utc),
                );
            if let Some(org) = inner_org {
                let cwd = env::current_dir()?;
                let snapshots =
                    acquire_org_from_github(&paths, &cwd, &org, started_at, mode.into())?;
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
            let started_at = started_at
                .as_deref()
                .map(DateTime::parse_from_rfc3339)
                .transpose()?
                .map_or_else(
                    || DateTime::<Utc>::from(SystemTime::now()),
                    |value| value.with_timezone(&Utc),
                );
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
    }
    Ok(())
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
