use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use directories::BaseDirs;
use ostrom_checks::{ActionFault, ActionRegistry, DoctorOptions, run_doctor, run_doctor_check};
use ostrom_core::{
    Catalogue, CatalogueEnumeration, CheckContractError, CheckDocument, CheckFault, RepositoryName,
    ResolvedCheck,
};
use ostrom_store::{
    AuditOptions, DispatchOutcome, DispatchRequest, ExecutableAssessmentDeriver, GateError,
    GateOptions, ImplementRequest, MigrationOutcome, OstromPaths, PassRequest, PassRole,
    PlanOptions, PublishDestination, PublishTarget, QueueDecision, ReplayOptions, SelectAction,
    SelectError, SelectOutcome, SelectRequest, SignalFlags, SweepError, SweepMode, SweepOptions,
    SweepParityOptions, TraceAppend, TraceView, UnavailableAssessmentDeriver, acquire_lease,
    acquire_org_from_github, append_trace_checked, audit, branch_name, create_work_order,
    decide_queue_item, encode_org_snapshots, encode_selection, grant_excuse, item_hash,
    lease_status, lint_queue_state, list_excuses, list_queue_json, local_drift, migrate,
    read_trace_json, release_lease, replay, run_dispatch, run_gate, run_implement, run_pass,
    run_plan, run_selection, run_sweep, run_sweep_parity, validate_lease_name,
    validate_work_order_file,
};

#[derive(Debug, Parser)]
#[command(name = "ostrom", version, about = "Ostrom workflow commons CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Diagnose the installed plugin, CLI, and local Ostrom state.
    Doctor {
        /// Run exactly one named doctor check.
        #[arg(long)]
        check: Option<String>,
    },
    /// Run one unattended delivery pass for a role.
    Pass { role: CliPassRole },
    /// Execute one durable work order in its item worktree.
    Implement {
        work_order_file: PathBuf,
        unit_name: String,
    },
    #[command(name = "__pass-worker", hide = true)]
    PassWorker {
        role: CliPassRole,
        supervisor_pid: u32,
    },
    #[command(name = "__implement-worker", hide = true)]
    ImplementWorker {
        work_order_file: PathBuf,
        unit_name: String,
        supervisor_pid: u32,
    },
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
    /// Append or read the machine-local sprint trace.
    Trace {
        #[command(subcommand)]
        command: TraceCommand,
    },
    /// Coordinate repeated role wakes with a named lease.
    Lease {
        #[command(subcommand)]
        command: LeaseCommand,
    },
    /// Create and validate durable implementation work orders.
    WorkOrder {
        #[command(subcommand)]
        command: WorkOrderCommand,
    },
    /// Move legacy Claude-hosted data to XDG config and state roots.
    Migrate,
    /// Compare native output with recorded legacy evidence in scratch state.
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
    /// Explain selector outcomes against merged pull requests and recorded state.
    Replay {
        /// Number of days in the merged-at window.
        #[arg(default_value_t = 30)]
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
    /// Approve one pending or deferred item.
    Approve { id: String },
    /// Reject and remove one pending or deferred item.
    Reject { id: String },
    /// Defer one pending item.
    Defer { id: String },
    /// Print selectors that did not match in the last sweep.
    Lint,
}

#[derive(Debug, Subcommand)]
enum TraceCommand {
    /// Append one fact/narration record.
    Append {
        kind: String,
        fact_json: String,
        narration_json: String,
    },
    /// Read facts while structurally omitting narration.
    Read,
    /// Read the principal-facing narration projection.
    ReadNarration,
}

#[derive(Debug, Subcommand)]
enum LeaseCommand {
    /// Acquire the configured named lease.
    Acquire {
        owner: String,
        ttl_seconds: Option<String>,
    },
    /// Release the configured named lease if the owner matches.
    Release { owner: String },
    /// Print the configured named lease.
    Status,
}

#[derive(Debug, Subcommand)]
enum WorkOrderCommand {
    /// Create or replace the durable order for one candidate.
    Create { candidate_json_file: PathBuf },
    /// Validate an existing schema-version 1 order.
    Validate { work_order_file: PathBuf },
    /// Hash an exact item identifier.
    ItemHash { item_id: String },
    /// Derive the branch name for an exact item identifier.
    BranchName { item_id: String },
}

#[derive(Debug, Subcommand)]
enum ParityCommand {
    /// Compare native sweep rows with recorded shell rows by id and field.
    Sweep {
        /// The clock used when the shell evidence was recorded.
        #[arg(long)]
        started_at: Option<String>,
        /// Recorded GitHub responses matching the shell evidence.
        #[arg(long)]
        fixture: PathBuf,
        /// Queue bytes recorded from the retired shell implementation.
        #[arg(long)]
        recorded_queue: PathBuf,
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliPassRole {
    Builder,
    Gatekeeper,
}

impl From<CliPassRole> for PassRole {
    fn from(value: CliPassRole) -> Self {
        match value {
            CliPassRole::Builder => Self::Builder,
            CliPassRole::Gatekeeper => Self::Gatekeeper,
        }
    }
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
    let paths = compatible_command_paths();
    match cli.command {
        Command::Doctor { check } => run_doctor_command(check)?,
        Command::Pass { role } => supervise(&["__pass-worker".into(), role_name(role).into()]),
        Command::Implement {
            work_order_file,
            unit_name,
        } => supervise(&[
            "__implement-worker".into(),
            work_order_file.into_os_string(),
            unit_name.into(),
        ]),
        Command::PassWorker {
            role,
            supervisor_pid,
        } => run_pass_worker(role, supervisor_pid),
        Command::ImplementWorker {
            work_order_file,
            unit_name,
            supervisor_pid,
        } => run_implement_worker(work_order_file, unit_name, supervisor_pid),
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
        Command::Queue { command } => match command {
            QueueCommand::List { format } => match format {
                OutputFormat::Json => {
                    if let Err(message) = state_root_present(&paths) {
                        exit_message(&message, 2);
                    }
                    match list_queue_json(&paths.queue_file()) {
                        Ok(output) => io::stdout().write_all(&output)?,
                        Err(_) => exit_message(
                            &format!(
                                "mandate queue: cannot read {}",
                                paths.queue_file().display()
                            ),
                            2,
                        ),
                    }
                }
            },
            QueueCommand::Lint => match lint_queue_state(&paths.sweep_state_file()) {
                Ok(output) => io::stdout().write_all(&output)?,
                Err(error) => exit_message(&error.to_string(), error.exit_code()),
            },
            QueueCommand::Approve { id } => {
                run_queue_decision(&paths, &id, QueueDecision::Approve)?
            }
            QueueCommand::Reject { id } => run_queue_decision(&paths, &id, QueueDecision::Reject)?,
            QueueCommand::Defer { id } => run_queue_decision(&paths, &id, QueueDecision::Defer)?,
        },
        Command::Trace { command } => match command {
            TraceCommand::Append {
                kind,
                fact_json,
                narration_json,
            } => {
                if kind.is_empty() {
                    exit_message("mandate trace: kind must not be empty", 2);
                }
                let fact = parse_json_object(&fact_json).unwrap_or_else(|| {
                    exit_message("mandate trace: fact-json must be a JSON object", 2)
                });
                let narration = parse_json_object(&narration_json).unwrap_or_else(|| {
                    exit_message("mandate trace: narration-json must be a JSON object", 2)
                });
                let timestamp = env::var("MANDATE_TRACE_TIME")
                    .ok()
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| {
                        DateTime::<Utc>::from(SystemTime::now())
                            .format("%Y-%m-%dT%H:%M:%SZ")
                            .to_string()
                    });
                let record = TraceAppend {
                    ts: timestamp,
                    kind,
                    fact,
                    narration,
                };
                match append_trace_checked(&paths.trace_file(), &paths.work_orders_dir(), &record) {
                    Ok(output) => io::stdout().write_all(&output)?,
                    Err(error) => exit_message(&error.to_string(), error.exit_code()),
                }
            }
            TraceCommand::Read => match read_trace_json(&paths.trace_file(), TraceView::Facts) {
                Ok(output) => io::stdout().write_all(&output)?,
                Err(error) => exit_message(&error.to_string(), error.exit_code()),
            },
            TraceCommand::ReadNarration => {
                match read_trace_json(&paths.trace_file(), TraceView::Narration) {
                    Ok(output) => io::stdout().write_all(&output)?,
                    Err(error) => exit_message(&error.to_string(), error.exit_code()),
                }
            }
        },
        Command::Lease { command } => run_lease_command(&paths, command)?,
        Command::WorkOrder { command } => run_work_order_command(&paths, command)?,
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
                    recorded_queue,
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
                recorded_queue,
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
                RepositoryName::new(repository)
                    .map(PublishDestination::explicit)
                    .map(PublishTarget::Explicit)
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
        Command::Replay { days } => {
            let replay_time = environment_time("MANDATE_REPLAY_TIME").unwrap_or_else(|message| {
                eprintln!("mandate replay: {message}");
                std::process::exit(2);
            });
            let working_directory = env::current_dir()?;
            match replay(&ReplayOptions {
                paths,
                working_directory,
                days,
                replay_time,
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

fn run_queue_decision(
    paths: &OstromPaths,
    id: &str,
    decision: QueueDecision,
) -> Result<(), Box<dyn std::error::Error>> {
    let event_time = env::var("MANDATE_EVENT_TIME")
        .ok()
        .filter(|value| !value.is_empty());
    match decide_queue_item(
        &paths.queue_file(),
        &paths.sweep_state_file(),
        &paths.selector_events_file(),
        id,
        decision,
        event_time.as_deref(),
    ) {
        Ok(output) => io::stdout().write_all(&output)?,
        Err(error) => exit_message(&error.to_string(), error.exit_code()),
    }
    Ok(())
}

fn parse_json_object(text: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()?
        .as_object()
        .cloned()
}

fn run_lease_command(
    paths: &OstromPaths,
    command: LeaseCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = env::var("MANDATE_LEASE_NAME")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "sprint.lease".to_owned());
    if let Err(error) = validate_lease_name(&name) {
        exit_message(&error.to_string(), error.exit_code());
    }
    match command {
        LeaseCommand::Acquire { owner, ttl_seconds } => {
            if owner.is_empty() {
                lease_usage();
            }
            let ttl = ttl_seconds
                .or_else(|| {
                    env::var("MANDATE_LEASE_TTL_SECONDS")
                        .ok()
                        .filter(|value| !value.is_empty())
                })
                .unwrap_or_else(|| "3600".to_owned());
            let ttl = parse_decimal_u64(&ttl)
                .filter(|value| *value > 0)
                .unwrap_or_else(|| {
                    exit_message("mandate lease: ttl-seconds must be a positive integer", 2)
                });
            let now_text = env::var("MANDATE_LEASE_NOW_EPOCH")
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |duration| duration.as_secs())
                        .to_string()
                });
            let now = parse_decimal_u64(&now_text).unwrap_or_else(|| {
                exit_message("mandate lease: current time must be Unix seconds", 2)
            });
            match acquire_lease(&paths.state, &name, &owner, now, ttl) {
                Ok(output) => io::stdout().write_all(&output)?,
                Err(error) => exit_message(&error.to_string(), error.exit_code()),
            }
        }
        LeaseCommand::Release { owner } => {
            if owner.is_empty() {
                lease_usage();
            }
            if let Err(error) = release_lease(&paths.state, &name, &owner) {
                exit_message(&error.to_string(), error.exit_code());
            }
        }
        LeaseCommand::Status => match lease_status(&paths.state, &name) {
            Ok(output) => io::stdout().write_all(&output)?,
            Err(error) => exit_message(&error.to_string(), error.exit_code()),
        },
    }
    Ok(())
}

fn parse_decimal_u64(value: &str) -> Option<u64> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn lease_usage() -> ! {
    exit_message(
        "usage: [MANDATE_LEASE_NAME=<name>] lease.sh acquire <owner> [ttl-seconds] | release <owner> | status",
        2,
    )
}

fn run_work_order_command(
    paths: &OstromPaths,
    command: WorkOrderCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        WorkOrderCommand::Create {
            candidate_json_file,
        } => {
            let created_at = env::var("MANDATE_TRACE_TIME")
                .ok()
                .filter(|value| !value.is_empty());
            let cost = env::var("MANDATE_ORDER_COST_CEILING_USD")
                .ok()
                .filter(|value| !value.is_empty());
            let tokens = env::var("MANDATE_ORDER_TOKEN_CEILING")
                .ok()
                .filter(|value| !value.is_empty());
            match create_work_order(
                &paths.state,
                &candidate_json_file,
                created_at.as_deref(),
                cost.as_deref(),
                tokens.as_deref(),
            ) {
                Ok(created) => {
                    if let Some(warning) = created.branch_warning {
                        eprintln!("{warning}");
                    }
                    println!("{}", created.target.display());
                }
                Err(error) => exit_message(&error.to_string(), error.exit_code()),
            }
        }
        WorkOrderCommand::Validate { work_order_file } => {
            if let Err(error) = validate_work_order_file(&work_order_file) {
                exit_message(&error.to_string(), error.exit_code());
            }
        }
        WorkOrderCommand::ItemHash { item_id } => {
            if item_id.is_empty() {
                work_order_usage();
            }
            println!("{}", item_hash(&item_id));
        }
        WorkOrderCommand::BranchName { item_id } => {
            if item_id.is_empty() {
                work_order_usage();
            }
            println!("{}", branch_name(&item_id));
        }
    }
    Ok(())
}

fn work_order_usage() -> ! {
    exit_message(
        "usage: work-order.sh create <candidate-json-file> | validate <work-order-file> | item-hash <item-id> | branch-name <item-id>",
        2,
    )
}

fn exit_message(message: &str, code: i32) -> ! {
    eprintln!("{message}");
    std::process::exit(code);
}

fn role_name(role: CliPassRole) -> &'static str {
    match role {
        CliPassRole::Builder => "builder",
        CliPassRole::Gatekeeper => "gatekeeper",
    }
}

// SIGHUP and SIGTERM do not exist on Windows, and signal-hook configures them
// out rather than stubbing them. The pass supervisor is a POSIX job-control
// mechanism; on Windows the flags stay unset and the supervisor simply waits
// for its worker, which is the same behaviour as a run that receives no signal.
#[cfg(unix)]
fn register_signals() -> io::Result<SignalFlags> {
    use signal_hook::{consts::signal, flag};

    let flags = SignalFlags::default();
    flag::register(signal::SIGHUP, flags.hup_flag())?;
    flag::register(signal::SIGINT, flags.int_flag())?;
    flag::register(signal::SIGTERM, flags.term_flag())?;
    Ok(flags)
}

#[cfg(not(unix))]
fn register_signals() -> io::Result<SignalFlags> {
    Ok(SignalFlags::default())
}

fn supervise(arguments: &[OsString]) -> ! {
    let signals = register_signals().unwrap_or_else(|error| {
        eprintln!("ostrom: could not install signal handlers: {error}");
        std::process::exit(1);
    });
    let executable = env::current_exe().unwrap_or_else(|error| {
        eprintln!("ostrom: could not resolve executable: {error}");
        std::process::exit(1);
    });
    let mut child = std::process::Command::new(executable)
        .args(arguments)
        .arg(std::process::id().to_string())
        .spawn()
        .unwrap_or_else(|error| {
            eprintln!("ostrom: could not start worker: {error}");
            std::process::exit(1);
        });
    loop {
        match child.try_wait() {
            Ok(Some(status)) => std::process::exit(status.code().unwrap_or(1)),
            Ok(None) => {}
            Err(error) => {
                eprintln!("ostrom: could not wait for worker: {error}");
                std::process::exit(1);
            }
        }
        if let Some(name) = signals.take_pending() {
            let command = if Path::new("/bin/kill").is_file() {
                "/bin/kill"
            } else {
                "kill"
            };
            let _ = std::process::Command::new(command)
                .args([format!("-{name}"), child.id().to_string()])
                .status();
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn run_pass_worker(role: CliPassRole, supervisor_pid: u32) -> ! {
    let signals = register_signals().unwrap_or_else(|error| {
        eprintln!("ostrom: could not install signal handlers: {error}");
        std::process::exit(1);
    });
    let claude_bin = env::var_os("CLAUDE_BIN").map_or_else(
        || {
            BaseDirs::new().map_or_else(
                || PathBuf::from("claude"),
                |dirs| dirs.home_dir().join(".local/bin/claude"),
            )
        },
        PathBuf::from,
    );
    let request = PassRequest {
        paths: compatible_command_paths(),
        role: role.into(),
        claude_bin,
        signals,
        supervisor_pid: Some(supervisor_pid),
    };
    match run_pass(&request) {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            if error.exit_code() != 0 {
                eprintln!("{error}");
            }
            std::process::exit(error.exit_code());
        }
    }
}

fn run_implement_worker(work_order_file: PathBuf, unit_name: String, supervisor_pid: u32) -> ! {
    let signals = register_signals().unwrap_or_else(|error| {
        eprintln!("ostrom: could not install signal handlers: {error}");
        std::process::exit(1);
    });
    let working_directory = env::current_dir().unwrap_or_else(|error| {
        eprintln!("ostrom implementer: could not resolve working directory: {error}");
        std::process::exit(1);
    });
    let plugin_root = env::var_os("OSTROM_PLUGIN_ROOT")
        .or_else(|| env::var_os("CLAUDE_PLUGIN_ROOT"))
        .map_or_else(|| working_directory.join("plugins/ostrom"), PathBuf::from);
    let request = ImplementRequest {
        paths: compatible_command_paths(),
        working_directory,
        plugin_root,
        order_file: work_order_file,
        unit_name,
        signals,
        supervisor_pid: Some(supervisor_pid),
    };
    match run_implement(&request) {
        Ok(url) => {
            println!("{url}");
            std::process::exit(0);
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(error.code);
        }
    }
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

fn run_doctor_command(check: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let plugin_root = env::var_os("OSTROM_PLUGIN_ROOT")
        .or_else(|| env::var_os("CLAUDE_PLUGIN_ROOT"))
        .map_or_else(|| cwd.join("plugins/ostrom"), PathBuf::from);
    let options = DoctorOptions::from_environment(plugin_root);
    let output = if let Some(name) = check {
        match run_doctor_check(options, &name) {
            Ok(output) => output,
            Err(error) if error.name() == "doctor_unknown_check" => {
                eprintln!("{}", error.detail().unwrap_or("unknown doctor check"));
                std::process::exit(2);
            }
            Err(error) => return Err(error.into()),
        }
    } else {
        run_doctor(options)
    };
    io::stdout().write_all(output.as_bytes())?;
    Ok(())
}

/// Resolve the state root the way the shell did, for every command.
///
/// The store's own resolver deliberately refuses to fall through to an
/// operator's home when `OSTROM_HOME` is unset, so a test with an incomplete
/// fixture cannot read live data. That guarantee belongs in the store. It also
/// means the store alone cannot find the roster of an operator who has not run
/// `ostrom migrate` — which is every operator today, since the shell wrote to
/// `${CLAUDE_CONFIG_DIR:-$HOME/.claude}/ostrom` and nothing has moved it.
///
/// Resolving that legacy root belongs here, in the layer the operator invokes.
/// An explicit `CLAUDE_CONFIG_DIR` is honoured as given; the *implicit*
/// `$HOME/.claude/ostrom` fallback applies only when that directory exists, so
/// a fresh install still lands on XDG.
fn compatible_command_paths() -> OstromPaths {
    if env::var_os("OSTROM_HOME").is_some_and(|home| !home.to_string_lossy().trim().is_empty()) {
        return resolved_or_exit();
    }
    // Empty means unset, matching the shell's `${CLAUDE_CONFIG_DIR:-...}`.
    // Without the filter an empty value resolves to the *relative* path
    // `ostrom/`, which reads whatever happens to be under the working
    // directory — the same defect `MANDATE_SECRETS_FILE` had. An explicit
    // non-empty value is honoured even if the directory does not exist yet,
    // because it is an instruction rather than a guess; the caller then gets a
    // named refusal that quotes it back.
    if let Some(config) =
        env::var_os("CLAUDE_CONFIG_DIR").filter(|value| !value.to_string_lossy().trim().is_empty())
    {
        return collapsed_root(PathBuf::from(config).join("ostrom"));
    }
    if let Some(base) = BaseDirs::new() {
        let legacy = base.home_dir().join(".claude/ostrom");
        if legacy.is_dir() {
            return collapsed_root(legacy);
        }
    }
    resolved_or_exit()
}

/// Refuse to report an empty queue from a state root that does not exist.
///
/// A missing root and an empty queue render identically — no rows, exit zero —
/// and the two mean opposite things. `ostrom queue list` read the wrong root
/// against a live 130-row queue and reported nothing at all, which is the same
/// failure `select-work.sh` had in production: a broken read that looks like a
/// quiet portfolio. Naming the directory it looked in is the whole fix.
fn state_root_present(paths: &OstromPaths) -> Result<(), String> {
    if paths.state.is_dir() {
        return Ok(());
    }
    Err(format!(
        "mandate queue: no Ostrom state root at {}; run a sweep, or set OSTROM_HOME or CLAUDE_CONFIG_DIR",
        paths.state.display()
    ))
}

fn collapsed_root(root: PathBuf) -> OstromPaths {
    OstromPaths {
        config: root.clone(),
        state: root,
    }
}

fn resolved_or_exit() -> OstromPaths {
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
    let registry = ActionRegistry::core(plugin_root.to_owned())?;
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
