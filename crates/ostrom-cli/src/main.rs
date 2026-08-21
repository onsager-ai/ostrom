use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::Duration,
};

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use directories::BaseDirs;
use ostrom_checks::{
    ActionFault, ActionRegistry, DoctorOptions, PreparedCheck, check_shell_retirement,
    check_skill_version_bump, generate_operation_settings, render_loop_units, run_doctor,
    run_doctor_check,
};
use ostrom_core::{
    CHECK_STORE_SCHEMA_VERSION, Catalogue, CatalogueEnumeration, CheckContractError, CheckDocument,
    CheckFault, CheckRun, CheckRunId, CheckState, CheckVerdict, InconclusivePolicy,
    OperationAction, RepositoryName, ResolvedCheck, ResolvedLoopCeilings, SelectorPrefix,
};
use ostrom_store::{
    AssessmentHarness, AuditOptions, Clock, DigestOptions, DispatchOutcome, DispatchRequest,
    ExecutableAssessmentDeriver, GateError, GateOptions, HarnessAssessmentDeriver,
    ImplementRequest, JsonlCheckStore, MigrationOutcome, OstromPaths, PassRequest, PassRole,
    PlanOptions, PublishDestination, PublishTarget, QueueDecision, ReplayOptions, SelectAction,
    SelectError, SelectOutcome, SelectRequest, SignalFlags, SweepError, SweepMode, SweepOptions,
    SweepParityOptions, TraceAppend, TraceView, UnavailableAssessmentDeriver, acquire_lease,
    acquire_org_from_github, append_trace_checked, audit, branch_name, clear_work_order,
    create_work_order, credential_output, decide_queue_item, encode_org_snapshots,
    encode_selection, environment, finalize_exited_implementer, grant_excuse, item_hash,
    lease_status, lint_queue_state, list_excuses, list_queue_json, local_drift, migrate,
    read_trace_json, release_lease, render_constitution, render_digest, replay, run_dispatch,
    run_gate, run_implement, run_pass, run_plan, run_repair_prs, run_selection, run_sweep,
    run_sweep_parity, validate_lease_name, validate_work_order_file,
};

mod operation_dispatch;
mod policy_manifest;

use operation_dispatch::{
    OperationDispatchError, OperationRuntime, ResolvedOperationTarget, dispatch_operation,
    manifest_path, parse_invocation, resolve_repository_target,
};

#[derive(Debug, Parser)]
#[command(name = "ostrom", version, about = "Ostrom workflow commons CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Sign a fully composed Ostrom policy manifest.
    Sign {
        /// Stable principal ID; its public key is installed as <KEY_ID>.pem.
        #[arg(long)]
        key_id: String,
        /// RSA private key in PKCS#8 or PKCS#1 PEM form.
        #[arg(long)]
        key: PathBuf,
        manifest: PathBuf,
    },
    /// Parse and validate an Ostrom policy manifest.
    Validate {
        /// Print the fully composed scalar/list-normalized manifest.
        #[arg(long)]
        normalized: bool,
        manifest: PathBuf,
    },
    /// List the operations declared by the active policy manifest.
    Operations {
        /// Restrict the list to operations granted somewhere to this actor.
        #[arg(long)]
        actor: Option<String>,
        /// Render the generated settings profile for this actor.
        #[arg(long, conflicts_with = "check_settings")]
        settings: Option<String>,
        /// Refuse when this settings file differs from the derived profile.
        #[arg(long, requires = "actor", conflicts_with = "settings")]
        check_settings: Option<PathBuf>,
    },
    /// Explain how authored policy resolves for one pull request.
    Explain {
        target: String,
        /// Policy manifest; defaults to repository then user configuration.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Actor projection to explain.
        #[arg(long, default_value = "builder")]
        actor: String,
        /// Operation projection to explain.
        #[arg(long, default_value = "work")]
        operation: String,
        /// Recorded sweep responses for a hermetic explanation.
        #[arg(long, hide = true)]
        fixture: Option<PathBuf>,
        /// Observation clock for hermetic replay.
        #[arg(long, hide = true)]
        started_at: Option<String>,
    },
    /// Run one declared policy loop.
    Loop {
        #[command(subcommand)]
        command: LoopCommand,
    },
    /// Render or check the systemd artifacts for declared loops.
    Loops {
        #[command(subcommand)]
        command: LoopsCommand,
    },
    /// Run one command with a scoped GitHub App installation credential.
    Credential {
        role: String,
        repository: String,
        /// Comma-separated owner/repository scope for the installation token.
        #[arg(long)]
        repositories: String,
        /// Comma-separated permission:level scope for the installation token.
        #[arg(long)]
        permissions: String,
        /// Command and arguments; `--` is required before this tail.
        #[arg(required = true, num_args = 1.., last = true)]
        child: Vec<OsString>,
    },
    /// Diagnose the installed plugin, CLI, and local Ostrom state.
    Doctor {
        /// Run exactly one named doctor check.
        #[arg(long)]
        check: Option<String>,
    },
    /// Run repository policy checks used by continuous integration.
    Check {
        #[command(subcommand)]
        command: CheckCommand,
    },
    /// Print the resolved mandate roster as JSON.
    Config,
    /// Merge base branches into eligible stale builder pull requests.
    RepairPrs {
        #[arg(allow_hyphen_values = true)]
        builder_lease_owner: Vec<String>,
    },
    /// Run a Claude Code hook entrypoint.
    Hook {
        #[command(subcommand)]
        command: HookCommand,
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
        /// Assess with a named harness; omission of the value selects claude.
        #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "claude")]
        assessor: Option<CliAssessmentHarness>,
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
    /// Invoke one policy operation. Actions are never direct CLI commands.
    #[command(external_subcommand)]
    Operation(Vec<OsString>),
}

#[derive(Debug, Subcommand)]
enum CheckCommand {
    /// Execute authored criteria and append their receipts to the check journal.
    Run,
    /// Require the shipped plugin wiring and skill protocols to agree with the CLI.
    PluginSurface,
    /// Prevent shell implementation files from reappearing.
    ShellRetirement,
    /// Require changed shipped plugin content to carry a plugin version bump.
    SkillVersionBump {
        #[arg(long)]
        base: String,
        #[arg(long)]
        head: String,
    },
}

#[derive(Debug, Subcommand)]
enum LoopCommand {
    /// Dispatch the operation bound to one named loop.
    Run { name: String },
}

#[derive(Debug, Subcommand)]
enum LoopsCommand {
    /// Write generated units without enabling, starting, or reloading them.
    Render {
        /// Artifact directory; defaults to the Ostrom config root's `systemd` directory.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Refuse when installed units differ from the generated artifacts.
    Check { installed: PathBuf },
}

#[derive(Debug, Subcommand)]
enum HookCommand {
    /// Emit the layered constitution for SessionStart.
    SessionStart,
    /// Render and acknowledge the durable queue digest.
    Digest,
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
    /// Append work-failed for one named stranded order.
    Clear { identifier: String },
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
enum CliAssessmentHarness {
    Claude,
    Codex,
    Copilot,
}

impl From<CliAssessmentHarness> for AssessmentHarness {
    fn from(value: CliAssessmentHarness) -> Self {
        match value {
            CliAssessmentHarness::Claude => Self::Claude,
            CliAssessmentHarness::Codex => Self::Codex,
            CliAssessmentHarness::Copilot => Self::Copilot,
        }
    }
}

fn resolve_plan_deriver(
    assessor: Option<CliAssessmentHarness>,
) -> Box<dyn ostrom_store::AssessmentDeriver> {
    if let Some(harness) = assessor.map(AssessmentHarness::from) {
        return named_plan_deriver(harness);
    }
    let Some(configured) = environment::OSTROM_PLAN_DERIVER.value_os() else {
        return Box::new(UnavailableAssessmentDeriver);
    };
    let harness = match configured.to_str() {
        Some("claude") => Some(AssessmentHarness::Claude),
        Some("codex") => Some(AssessmentHarness::Codex),
        Some("copilot") => Some(AssessmentHarness::Copilot),
        _ => None,
    };
    harness.map_or_else(
        || {
            Box::new(ExecutableAssessmentDeriver::new(PathBuf::from(configured)))
                as Box<dyn ostrom_store::AssessmentDeriver>
        },
        named_plan_deriver,
    )
}

fn named_plan_deriver(harness: AssessmentHarness) -> Box<dyn ostrom_store::AssessmentDeriver> {
    let variable = match harness {
        AssessmentHarness::Claude => environment::CLAUDE_BIN,
        AssessmentHarness::Codex => environment::CODEX_BIN,
        AssessmentHarness::Copilot => environment::COPILOT_BIN,
    };
    let executable = variable
        .value_os()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(harness.name()));
    Box::new(HarnessAssessmentDeriver::new(harness, executable))
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
    let clock = Clock::realtime();
    let paths = compatible_command_paths();
    match cli.command {
        Command::Sign {
            key_id,
            key,
            manifest,
        } => policy_manifest::run_sign(&manifest, &key_id, &key)?,
        Command::Validate {
            normalized,
            manifest,
        } => policy_manifest::run_validate(&manifest, normalized)?,
        Command::Operations {
            actor,
            settings,
            check_settings,
        } => run_operations_command(
            &paths,
            actor.as_deref(),
            settings.as_deref(),
            check_settings.as_deref(),
        )?,
        Command::Explain {
            target,
            manifest,
            actor,
            operation,
            fixture,
            started_at,
        } => {
            let observed_at = resolve_started_at(started_at.as_deref(), &clock)?;
            let cwd = env::current_dir()?;
            let output = policy_manifest::run_explain(&policy_manifest::ExplainOptions {
                paths: &paths,
                working_directory: &cwd,
                target: &target,
                manifest: manifest.as_deref(),
                fixture: fixture.as_deref(),
                observed_at,
                actor: &actor,
                operation: &operation,
            })?;
            io::stdout().write_all(output.as_bytes())?;
        }
        Command::Loop { command } => match command {
            LoopCommand::Run { name } => run_loop_command(&paths, &name)?,
        },
        Command::Loops { command } => run_loops_command(&paths, command)?,
        Command::Credential {
            role,
            repository,
            repositories,
            permissions,
            child,
        } => match credential_output(
            &paths,
            &role,
            &repository,
            &repositories,
            &permissions,
            &child,
        ) {
            Ok(output) => {
                io::stdout().write_all(&output.stdout)?;
                io::stderr().write_all(&output.stderr)?;
                if !output.status.success() {
                    std::process::exit(output.status.code().unwrap_or(1));
                }
            }
            Err(error) => exit_message(&format!("ostrom credential: {error}"), error.exit_code()),
        },
        Command::Doctor { check } => run_doctor_command(check, &clock)?,
        Command::Check {
            command: CheckCommand::Run,
        } => {
            let cwd = env::current_dir()?;
            let plugin_root = environment::OSTROM_PLUGIN_ROOT
                .value_os()
                .or_else(|| environment::CLAUDE_PLUGIN_ROOT.value_os())
                .map_or_else(|| cwd.join("plugins/ostrom"), PathBuf::from);
            let resolutions = resolve_plan_checks(&paths, &cwd, &plugin_root)?;
            if let Some(fault) = &resolutions.catalogue_fault {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("ostrom check run: {}", fault.name),
                )
                .into());
            }
            let outcome =
                execute_prepared_checks(&paths, &resolutions, CriteriaSelection::All, clock.now())?;
            println!(
                "ostrom check run: {} passed; {} failed; {} inconclusive; {} faulted; wrote {}",
                outcome.passed,
                outcome.failed,
                outcome.inconclusive,
                outcome.faulted,
                paths.check_journal_file().display()
            );
            for warning in &outcome.warnings {
                eprintln!("warning: {warning}");
            }
            if outcome.failed != 0 || outcome.blocked != 0 || outcome.faulted != 0 {
                std::process::exit(1);
            }
        }
        Command::Check {
            command: CheckCommand::PluginSurface,
        } => {
            let report = ostrom_checks::check_plugin_surface(&env::current_dir()?)?;
            if !report.is_clean() {
                eprint!("{report}");
                std::process::exit(1);
            }
        }
        Command::Check {
            command: CheckCommand::ShellRetirement,
        } => {
            let report = check_shell_retirement(&env::current_dir()?)?;
            if !report.is_clean() {
                eprintln!("{report}");
                std::process::exit(1);
            }
        }
        Command::Check {
            command: CheckCommand::SkillVersionBump { base, head },
        } => {
            let report = check_skill_version_bump(&env::current_dir()?, &base, &head)?;
            for violation in &report.violations {
                eprintln!(
                    "skill version check: plugin '{}' changed shipped file '{}' without changing version in {} (still {}); the cache is keyed by version, so this change would never reach an installed session",
                    violation.plugin,
                    violation.shipped_path.display(),
                    violation.manifest.display(),
                    violation.version
                );
            }
            if !report.is_clean() {
                std::process::exit(1);
            }
        }
        Command::Config => {
            let config = ostrom_store::load_config_or_defaults(&paths, &env::current_dir()?)
                .unwrap_or_else(|error| exit_message(&error.to_string(), 2));
            let mut config = serde_json::to_value(config)?;
            if let Some(projects) = config
                .get_mut("projects")
                .and_then(serde_json::Value::as_array_mut)
            {
                for project in projects {
                    if project
                        .get("max_implementers_per_repository")
                        .is_some_and(serde_json::Value::is_null)
                    {
                        project
                            .as_object_mut()
                            .expect("serialized project is an object")
                            .remove("max_implementers_per_repository");
                    }
                }
            }
            serde_json::to_writer(io::stdout(), &config)?;
            println!();
        }
        Command::RepairPrs {
            builder_lease_owner,
        } => {
            if builder_lease_owner.len() != 1 || builder_lease_owner[0].is_empty() {
                exit_message("usage: repair-prs.sh <builder-lease-owner>", 2);
            }
            let output = match run_repair_prs(&ostrom_store::RepairOptions {
                paths,
                working_directory: env::current_dir()?,
                lease_owner: builder_lease_owner[0].clone(),
                clock: clock.clone(),
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
        Command::Hook { command } => match command {
            HookCommand::SessionStart => {
                let cwd = env::current_dir().unwrap_or_default();
                let plugin_root = environment::CLAUDE_PLUGIN_ROOT
                    .value_os()
                    .map_or_else(|| cwd.join("plugins/ostrom"), PathBuf::from);
                let home = environment::HOME
                    .value_os()
                    .map_or_else(PathBuf::new, PathBuf::from);
                let output = render_constitution(&plugin_root, &paths.config, &cwd, &home);
                io::stdout().write_all(output.as_bytes())?;
            }
            HookCommand::Digest => {
                let output = render_digest(&DigestOptions {
                    paths,
                    working_directory: env::current_dir().unwrap_or_default(),
                    clock: clock.clone(),
                });
                io::stdout().write_all(output.stdout.as_bytes())?;
                io::stderr().write_all(output.stderr.as_bytes())?;
            }
        },
        Command::Pass { role } => supervise(
            &["__pass-worker".into(), role_name(role).into()],
            None,
            &clock,
        ),
        Command::Implement {
            work_order_file,
            unit_name,
        } => {
            let arguments = [
                "__implement-worker".into(),
                work_order_file.clone().into_os_string(),
                unit_name.clone().into(),
            ];
            supervise(&arguments, Some((&work_order_file, &unit_name)), &clock)
        }
        Command::PassWorker {
            role,
            supervisor_pid,
        } => run_pass_worker(role, supervisor_pid, clock),
        Command::ImplementWorker {
            work_order_file,
            unit_name,
            supervisor_pid,
        } => run_implement_worker(work_order_file, unit_name, supervisor_pid, clock),
        Command::Dispatch { arguments } => {
            run_dispatch_command(arguments, clock);
        }
        Command::SelectWork { arguments } => {
            run_select_work(arguments, clock);
        }
        Command::Gate { target } => {
            if target.len() != 1 {
                let error = GateError::InvalidTarget;
                eprintln!("{error}");
                std::process::exit(error.exit_code());
            }
            let output = match run_gate(&GateOptions {
                paths,
                working_directory: env::current_dir()?,
                target: target[0].clone(),
                timestamp: clock.timestamp(),
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
                run_queue_decision(&paths, &id, QueueDecision::Approve, &clock)?
            }
            QueueCommand::Reject { id } => {
                run_queue_decision(&paths, &id, QueueDecision::Reject, &clock)?
            }
            QueueCommand::Defer { id } => {
                run_queue_decision(&paths, &id, QueueDecision::Defer, &clock)?
            }
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
                let record = TraceAppend {
                    ts: clock.timestamp(),
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
        Command::Lease { command } => run_lease_command(&paths, command, &clock)?,
        Command::WorkOrder { command } => run_work_order_command(&paths, command, &clock)?,
        Command::Migrate => {
            let legacy = legacy_home()?;
            match migrate(&legacy, &paths, clock.epoch_seconds())? {
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
            let started_at = resolve_started_at(started_at.as_deref(), &clock)?;
            let cwd = env::current_dir()?;
            let executable = env::current_exe()?;
            let plugin_root = environment::OSTROM_PLUGIN_ROOT
                .value_os()
                .or_else(|| environment::CLAUDE_PLUGIN_ROOT.value_os())
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
            let started_at = resolve_started_at(started_at.as_deref(), &clock)?;
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
            let plugin_root = environment::OSTROM_PLUGIN_ROOT
                .value_os()
                .or_else(|| environment::CLAUDE_PLUGIN_ROOT.value_os())
                .map_or_else(|| cwd.join("plugins/ostrom"), PathBuf::from);
            let policy = policy_manifest::load_optional_bundle(&paths, &cwd)?;
            let outcome = run_sweep(&SweepOptions {
                paths,
                working_directory: cwd,
                executable,
                plugin_root,
                started_at,
                requested_mode: mode.into(),
                fixture,
                publish,
                policy,
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
            assessor,
            fixture,
            started_at,
        } => {
            let started_at = resolve_started_at(started_at.as_deref(), &clock)?;
            let cwd = env::current_dir()?;
            let executable = env::current_exe()?;
            let plugin_root = environment::OSTROM_PLUGIN_ROOT
                .value_os()
                .or_else(|| environment::CLAUDE_PLUGIN_ROOT.value_os())
                .map_or_else(|| cwd.join("plugins/ostrom"), PathBuf::from);
            let policy = policy_manifest::load_optional_bundle(&paths, &cwd)?;
            let check_resolutions = resolve_plan_checks(&paths, &cwd, &plugin_root)?;
            execute_prepared_checks(
                &paths,
                &check_resolutions,
                CriteriaSelection::StaleOrNever,
                started_at,
            )?;
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
                    policy,
                },
                resolved_checks: check_resolutions.resolved,
                check_resolution_faults: check_resolutions.faults,
                catalogue_fault: check_resolutions.catalogue_fault,
            };
            let mut deriver = resolve_plan_deriver(assessor);
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
            let working_directory = env::current_dir()?;
            match audit(&AuditOptions {
                paths,
                working_directory,
                days,
                audit_time: clock.now(),
            }) {
                Ok(output) => io::stdout().write_all(output.as_bytes())?,
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(error.exit_code());
                }
            }
        }
        Command::Replay { days } => {
            let working_directory = env::current_dir()?;
            match replay(&ReplayOptions {
                paths,
                working_directory,
                days,
                replay_time: clock.now(),
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
            } => match grant_excuse(&paths, &target, &condition, &reason, Some(clock.now())) {
                Ok(output) => io::stdout().write_all(output.as_bytes())?,
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(error.exit_code());
                }
            },
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
        Command::Operation(arguments) => run_operation_command(&paths, &arguments)?,
    }
    Ok(())
}

fn run_operations_command(
    paths: &OstromPaths,
    actor: Option<&str>,
    settings_actor: Option<&str>,
    check_settings: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let manifest = policy_manifest::load(&manifest_path(&paths.config))?;
    if let Some(settings_actor) = settings_actor {
        print!(
            "{}",
            generate_operation_settings(&manifest, settings_actor)?
        );
        return Ok(());
    }
    if let Some(path) = check_settings {
        let actor = actor.expect("clap requires actor with check-settings");
        if let Some(drift) = ostrom_checks::check_operation_settings_drift(&manifest, actor, path)?
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, drift.detail).into());
        }
        println!("valid: {}", path.display());
        return Ok(());
    }
    if let Some(actor) = actor {
        if !manifest.actors.contains_key(actor) {
            return Err(OperationDispatchError::UnknownActor(actor.to_owned()).into());
        }
    }
    for (name, operation) in &manifest.operations {
        let visible = actor.is_none_or(|actor| {
            manifest.grants.values().any(|grant| {
                (grant.actors.is_empty() || grant.actors.iter().any(|value| value == actor))
                    && (grant.operations.is_empty()
                        || grant.operations.iter().any(|value| value == name))
            })
        });
        if visible {
            println!(
                "{name}\t{}",
                operation.name.as_deref().unwrap_or(name.as_str())
            );
        }
    }
    Ok(())
}

fn run_loops_command(
    paths: &OstromPaths,
    command: LoopsCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    let manifest = policy_manifest::load(&manifest_path(&paths.config))?;
    match command {
        LoopsCommand::Render { output } => {
            let output = output.unwrap_or_else(|| paths.config.join("systemd"));
            for path in render_loop_units(&manifest, &output)? {
                println!("{}", path.display());
            }
        }
        LoopsCommand::Check { installed } => {
            let drift = ostrom_checks::check_loop_units_drift(&manifest, &installed)?;
            if !drift.is_clean() {
                return Err(LoopCommandError::Drift {
                    installed,
                    missing: drift.missing,
                    changed: drift.changed,
                    unexpected: drift.unexpected,
                }
                .into());
            }
            println!("valid: {}", installed.display());
        }
    }
    Ok(())
}

fn run_loop_command(paths: &OstromPaths, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let manifest = policy_manifest::load(&manifest_path(&paths.config))?;
    let resolved = manifest.resolve_loop(name)?;
    assert_loop_environment(&resolved.actor, resolved.ceilings)?;
    let invocation = operation_dispatch::OperationInvocation {
        name: resolved.operation.clone(),
        target: resolved.target,
        parameters: resolved.parameters,
    };
    let working_directory = env::current_dir()?;
    let plugin_root = env::var_os("OSTROM_PLUGIN_ROOT")
        .or_else(|| env::var_os("CLAUDE_PLUGIN_ROOT"))
        .map_or_else(|| working_directory.join("plugins/ostrom"), PathBuf::from);
    let selector_prefixes =
        operation_selector_prefixes(&manifest, &resolved.actor, &invocation.name);
    let mut runtime = CliOperationRuntime {
        paths,
        actor: &resolved.actor,
        working_directory: &working_directory,
        plugin_root: &plugin_root,
        selector_prefixes,
        ceilings: Some(resolved.ceilings),
    };
    dispatch_operation(&manifest, &resolved.actor, &invocation, &mut runtime)?;
    Ok(())
}

fn run_operation_command(
    paths: &OstromPaths,
    arguments: &[OsString],
) -> Result<(), Box<dyn std::error::Error>> {
    let manifest = policy_manifest::load(&manifest_path(&paths.config))?;
    let invocation = parse_invocation(&manifest, arguments)?;
    let actor = ostrom_store::environment::OSTROM_ACTOR
        .value()
        .filter(|actor| !actor.trim().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "OSTROM_ACTOR is required"))?;
    let working_directory = env::current_dir()?;
    let plugin_root = ostrom_store::environment::OSTROM_PLUGIN_ROOT
        .value_os()
        .or_else(|| ostrom_store::environment::CLAUDE_PLUGIN_ROOT.value_os())
        .map_or_else(|| working_directory.join("plugins/ostrom"), PathBuf::from);
    let selector_prefixes = operation_selector_prefixes(&manifest, &actor, &invocation.name);
    let mut runtime = CliOperationRuntime {
        paths,
        actor: &actor,
        working_directory: &working_directory,
        plugin_root: &plugin_root,
        selector_prefixes,
        ceilings: None,
    };
    dispatch_operation(&manifest, &actor, &invocation, &mut runtime)?;
    Ok(())
}

struct CliOperationRuntime<'a> {
    paths: &'a OstromPaths,
    actor: &'a str,
    working_directory: &'a Path,
    plugin_root: &'a Path,
    selector_prefixes: BTreeSet<SelectorPrefix>,
    ceilings: Option<ResolvedLoopCeilings>,
}

impl OperationRuntime for CliOperationRuntime<'_> {
    fn resolve_target(
        &mut self,
        raw: &str,
        actor: &str,
        operation: &str,
    ) -> Result<ResolvedOperationTarget, OperationDispatchError> {
        let mut target = resolve_repository_target(raw, actor, operation)?;
        if self.selector_prefixes.iter().any(|prefix| {
            matches!(
                prefix,
                SelectorPrefix::Label | SelectorPrefix::Path | SelectorPrefix::Type
            )
        }) {
            resolve_target_metadata(self.paths, actor, &mut target)?;
        }
        Ok(target)
    }

    fn require(
        &mut self,
        check: &str,
        _target: &ResolvedOperationTarget,
    ) -> Result<(), OperationDispatchError> {
        execute_operation_requirement(self.paths, self.working_directory, self.plugin_root, check)
    }

    fn execute(
        &mut self,
        action: &'static OperationAction,
        target: &ResolvedOperationTarget,
        parameters: &BTreeMap<String, serde_yaml::Value>,
    ) -> Result<(), OperationDispatchError> {
        execute_operation_action(
            self.paths,
            self.actor,
            action,
            target,
            parameters,
            self.ceilings,
        )
    }
}

fn operation_selector_prefixes(
    manifest: &ostrom_core::PolicyManifest,
    actor: &str,
    operation: &str,
) -> BTreeSet<SelectorPrefix> {
    manifest
        .grants
        .values()
        .chain(manifest.denies.values())
        .filter(|rule| {
            (rule.actors.is_empty() || rule.actors.iter().any(|candidate| candidate == actor))
                && (rule.operations.is_empty()
                    || rule
                        .operations
                        .iter()
                        .any(|candidate| candidate == operation))
        })
        .flat_map(|rule| {
            rule.selectors
                .iter()
                .map(ostrom_core::PolicySelector::prefix)
        })
        .collect()
}

fn resolve_target_metadata(
    paths: &OstromPaths,
    actor: &str,
    target: &mut ResolvedOperationTarget,
) -> Result<(), OperationDispatchError> {
    if !target.raw.contains('#') {
        return Err(OperationDispatchError::TargetResolutionFailed {
            target: target.raw.clone(),
            message: "selector-constrained operations require a pull request target".to_owned(),
        });
    }
    let command = [
        "gh",
        "pr",
        "view",
        target.raw.as_str(),
        "--json",
        "labels,files,title",
    ];
    let output = credential_output(
        paths,
        actor,
        &target.repository,
        &target.repository,
        "contents:read,pull_requests:read",
        &command,
    )
    .map_err(|error| OperationDispatchError::TargetResolutionFailed {
        target: target.raw.clone(),
        message: error.to_string(),
    })?;
    if !output.status.success() {
        return Err(OperationDispatchError::TargetResolutionFailed {
            target: target.raw.clone(),
            message: format!("gh pr view exited with {}", output.status),
        });
    }
    let document = serde_json::from_slice::<serde_json::Value>(&output.stdout).map_err(|_| {
        OperationDispatchError::TargetResolutionFailed {
            target: target.raw.clone(),
            message: "gh pr view returned malformed JSON".to_owned(),
        }
    })?;
    target.candidate.labels = document
        .get("labels")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|label| label.get("name").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect();
    target.candidate.paths = document
        .get("files")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|file| file.get("path").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect();
    target.candidate.commit_type = document
        .get("title")
        .and_then(serde_json::Value::as_str)
        .and_then(commit_type);
    Ok(())
}

fn commit_type(title: &str) -> Option<String> {
    let prefix = title.split_once(':')?.0.trim_end_matches('!');
    let kind = prefix.split_once('(').map_or(prefix, |(kind, _)| kind);
    (!kind.is_empty()
        && kind
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then(|| kind.to_ascii_lowercase())
}

fn execute_operation_requirement(
    paths: &OstromPaths,
    working_directory: &Path,
    plugin_root: &Path,
    check: &str,
) -> Result<(), OperationDispatchError> {
    let resolutions =
        resolve_plan_checks(paths, working_directory, plugin_root).map_err(|error| {
            OperationDispatchError::RequirementFailed(format!("{check}: {}", error.name()))
        })?;
    if let Some(fault) = resolutions.catalogue_fault {
        return Err(OperationDispatchError::RequirementFailed(format!(
            "{check}: {}",
            fault.name
        )));
    }
    if let Some(fault) = resolutions.faults.get(check) {
        return Err(OperationDispatchError::RequirementFailed(format!(
            "{check}: {}",
            fault.name
        )));
    }
    let prepared = resolutions
        .prepared
        .get(check)
        .ok_or_else(|| OperationDispatchError::RequirementFailed(check.to_owned()))?;
    // Operation guards are observations made immediately before their action.
    // Production intentionally owns this real clock; hermetic dispatcher tests
    // use a fake runtime rather than pinning a path production leaves unpinned.
    let receipt = prepared.execute(&format!("operation:{check}"));
    let passed = receipt.verdict == Some(CheckVerdict::Pass) && receipt.error.is_none();
    let mut store = JsonlCheckStore::new(paths);
    store
        .append_run(&CheckRun {
            schema_version: CHECK_STORE_SCHEMA_VERSION,
            run_id: new_check_run_id(receipt.completed_at),
            completed_at: receipt.completed_at.to_rfc3339(),
            receipts: vec![receipt],
        })
        .map_err(|error| OperationDispatchError::RequirementFailed(format!("{check}: {error}")))?;
    if passed {
        Ok(())
    } else {
        Err(OperationDispatchError::RequirementFailed(check.to_owned()))
    }
}

fn execute_operation_action(
    paths: &OstromPaths,
    actor: &str,
    action: &'static OperationAction,
    target: &ResolvedOperationTarget,
    parameters: &BTreeMap<String, serde_yaml::Value>,
    ceilings: Option<ResolvedLoopCeilings>,
) -> Result<(), OperationDispatchError> {
    match action.uses {
        "gh/post-verdict" => {
            let note = operation_string(parameters, "note", action.uses)?;
            run_mediated(
                paths,
                actor,
                action,
                target,
                &["gh", "pr", "comment", &target.raw, "--body", note],
            )
        }
        "gh/merge-pr" => {
            let method = parameters
                .get("method")
                .and_then(serde_yaml::Value::as_str)
                .unwrap_or("squash");
            if !matches!(method, "merge" | "rebase" | "squash") {
                return Err(action_failed(
                    action.uses,
                    "method must be merge, rebase, or squash",
                ));
            }
            let method_flag = format!("--{method}");
            run_mediated(
                paths,
                actor,
                action,
                target,
                &["gh", "pr", "merge", &target.raw, &method_flag],
            )
        }
        "git/tag" => {
            let name = operation_string(parameters, "name", action.uses)?;
            let mut tag = ProcessCommand::new("git");
            if let Some(message) = parameters
                .get("message")
                .and_then(serde_yaml::Value::as_str)
            {
                tag.args(["tag", "-a", name, "-m", message]);
            } else {
                tag.args(["tag", name]);
            }
            let status = tag
                .status()
                .map_err(|error| action_failed(action.uses, error))?;
            if !status.success() {
                return Err(action_failed(action.uses, "git tag rejected the tag"));
            }
            let reference = format!("refs/tags/{name}");
            run_mediated(
                paths,
                actor,
                action,
                target,
                &["git", "push", "origin", &reference],
            )
        }
        "cmd/run" => run_local_command(action, parameters, ceilings),
        _ => Err(OperationDispatchError::UnknownAction(
            action.uses.to_owned(),
        )),
    }
}

fn run_mediated(
    paths: &OstromPaths,
    actor: &str,
    action: &'static OperationAction,
    target: &ResolvedOperationTarget,
    command: &[&str],
) -> Result<(), OperationDispatchError> {
    let permissions = action
        .scopes
        .iter()
        .map(|scope| format!("{}:{}", scope.permission, scope.level))
        .collect::<Vec<_>>()
        .join(",");
    let output = credential_output(
        paths,
        actor,
        &target.repository,
        &target.repository,
        &permissions,
        command,
    )
    .map_err(|error| action_failed(action.uses, error))?;
    io::stdout()
        .write_all(&output.stdout)
        .map_err(|error| action_failed(action.uses, error))?;
    io::stderr()
        .write_all(&output.stderr)
        .map_err(|error| action_failed(action.uses, error))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(action_failed(
            action.uses,
            format!("child exited with {}", output.status),
        ))
    }
}

fn run_local_command(
    action: &'static OperationAction,
    parameters: &BTreeMap<String, serde_yaml::Value>,
    ceilings: Option<ResolvedLoopCeilings>,
) -> Result<(), OperationDispatchError> {
    let script = operation_string(parameters, "script", action.uses)?;
    let timeout = operation_timeout(parameters.get("timeout"), action.uses)?;
    let mut command = ProcessCommand::new("sh");
    command
        .args(["-c", script])
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(ceilings) = ceilings {
        apply_loop_ceilings(&mut command, ceilings);
    }
    let mut child = command
        .spawn()
        .map_err(|error| action_failed(action.uses, error))?;
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| action_failed(action.uses, error))?
        {
            return if status.success() {
                Ok(())
            } else {
                Err(action_failed(
                    action.uses,
                    format!("child exited with {status}"),
                ))
            };
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(action_failed(action.uses, "child timed out"));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn apply_loop_ceilings(command: &mut ProcessCommand, ceilings: ResolvedLoopCeilings) {
    if let Some(value) = ceilings.spend_usd {
        command.env("MANDATE_DAILY_CAP_USD", render_ceiling_number(value));
    }
    if let Some(value) = ceilings.concurrent {
        command.env("MANDATE_MAX_IMPLEMENTERS", value.to_string());
    }
    if let Some(value) = ceilings.tokens {
        command.env("MANDATE_ORDER_TOKEN_CEILING", value.to_string());
    }
}

fn assert_loop_environment(
    actor: &str,
    ceilings: ResolvedLoopCeilings,
) -> Result<(), LoopCommandError> {
    if let Some(enforced) = nonempty_env("OSTROM_ACTOR")
        && enforced != actor
    {
        return Err(LoopCommandError::ActorMismatch {
            declared: actor.to_owned(),
            enforced,
        });
    }
    assert_u64_ceiling(
        "MANDATE_MAX_IMPLEMENTERS",
        "concurrent",
        ceilings.concurrent,
    )?;
    assert_f64_ceiling("MANDATE_DAILY_CAP_USD", "spend_usd", ceilings.spend_usd)?;
    assert_u64_ceiling("MANDATE_ORDER_TOKEN_CEILING", "tokens", ceilings.tokens)
}

fn assert_u64_ceiling(
    variable: &'static str,
    field: &'static str,
    declared: Option<u64>,
) -> Result<(), LoopCommandError> {
    let Some(enforced) = nonempty_env(variable) else {
        return Ok(());
    };
    let matches = enforced.parse::<u64>().ok() == declared;
    if matches {
        Ok(())
    } else {
        Err(LoopCommandError::CeilingMismatch {
            field,
            variable,
            declared: declared.map_or_else(|| "unset".to_owned(), |value| value.to_string()),
            enforced,
        })
    }
}

fn assert_f64_ceiling(
    variable: &'static str,
    field: &'static str,
    declared: Option<f64>,
) -> Result<(), LoopCommandError> {
    let Some(enforced) = nonempty_env(variable) else {
        return Ok(());
    };
    let matches = enforced
        .parse::<f64>()
        .ok()
        .zip(declared)
        .is_some_and(|(enforced, declared)| enforced == declared);
    if matches {
        Ok(())
    } else {
        Err(LoopCommandError::CeilingMismatch {
            field,
            variable,
            declared: declared.map_or_else(|| "unset".to_owned(), render_ceiling_number),
            enforced,
        })
    }
}

fn nonempty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn render_ceiling_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

#[derive(Debug, thiserror::Error)]
enum LoopCommandError {
    #[error(
        "loop actor mismatch: manifest declares `{declared}` but OSTROM_ACTOR enforces `{enforced}`"
    )]
    ActorMismatch { declared: String, enforced: String },
    #[error(
        "loop ceiling mismatch for `{field}`: manifest declares `{declared}` but {variable} enforces `{enforced}`"
    )]
    CeilingMismatch {
        field: &'static str,
        variable: &'static str,
        declared: String,
        enforced: String,
    },
    #[error(
        "loop units at `{}` drift (missing: {}; changed: {}; unexpected: {})",
        installed.display(),
        format_names(missing),
        format_names(changed),
        format_names(unexpected)
    )]
    Drift {
        installed: PathBuf,
        missing: Vec<String>,
        changed: Vec<String>,
        unexpected: Vec<String>,
    },
}

fn format_names(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

fn operation_timeout(
    value: Option<&serde_yaml::Value>,
    action: &str,
) -> Result<Duration, OperationDispatchError> {
    let Some(value) = value else {
        return Ok(Duration::from_secs(30));
    };
    if let Some(seconds) = value.as_u64().filter(|seconds| *seconds > 0) {
        return Ok(Duration::from_secs(seconds));
    }
    let Some(value) = value.as_str() else {
        return Err(action_failed(action, "timeout must be a positive duration"));
    };
    let (number, factor) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1_u64)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else {
        return Err(action_failed(action, "timeout must end in ms, s, or m"));
    };
    let milliseconds = number
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .and_then(|number| number.checked_mul(factor))
        .ok_or_else(|| action_failed(action, "timeout must be a positive duration"))?;
    Ok(Duration::from_millis(milliseconds))
}

fn operation_string<'a>(
    parameters: &'a BTreeMap<String, serde_yaml::Value>,
    name: &str,
    action: &str,
) -> Result<&'a str, OperationDispatchError> {
    parameters
        .get(name)
        .and_then(serde_yaml::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| action_failed(action, format!("parameter `{name}` must be a string")))
}

fn action_failed(action: &str, message: impl std::fmt::Display) -> OperationDispatchError {
    OperationDispatchError::ActionFailed {
        action: action.to_owned(),
        message: message.to_string(),
    }
}

fn run_queue_decision(
    paths: &OstromPaths,
    id: &str,
    decision: QueueDecision,
    clock: &Clock,
) -> Result<(), Box<dyn std::error::Error>> {
    let event_time = clock.timestamp();
    match decide_queue_item(
        &paths.queue_file(),
        &paths.sweep_state_file(),
        &paths.selector_events_file(),
        id,
        decision,
        Some(&event_time),
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
    clock: &Clock,
) -> Result<(), Box<dyn std::error::Error>> {
    let name = environment::MANDATE_LEASE_NAME
        .value()
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
                    environment::MANDATE_LEASE_TTL_SECONDS
                        .value()
                        .filter(|value| !value.is_empty())
                })
                .unwrap_or_else(|| "3600".to_owned());
            let ttl = parse_decimal_u64(&ttl)
                .filter(|value| *value > 0)
                .unwrap_or_else(|| {
                    exit_message("mandate lease: ttl-seconds must be a positive integer", 2)
                });
            match acquire_lease(&paths.state, &name, &owner, clock.epoch_seconds(), ttl) {
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
    clock: &Clock,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        WorkOrderCommand::Create {
            candidate_json_file,
        } => {
            let cost = environment::MANDATE_ORDER_COST_CEILING_USD
                .value()
                .filter(|value| !value.is_empty());
            let tokens = environment::MANDATE_ORDER_TOKEN_CEILING
                .value()
                .filter(|value| !value.is_empty());
            match create_work_order(
                &paths.state,
                &candidate_json_file,
                clock,
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
        WorkOrderCommand::Clear { identifier } => {
            if identifier.is_empty() {
                work_order_usage();
            }
            match clear_work_order(&paths.state, &identifier, clock) {
                Ok(cleared) => println!("{} {}", cleared.order_id, cleared.item_id),
                Err(error) => exit_message(&error.to_string(), error.exit_code()),
            }
        }
    }
    Ok(())
}

fn work_order_usage() -> ! {
    exit_message(
        "usage: work-order.sh create <candidate-json-file> | validate <work-order-file> | item-hash <item-id> | branch-name <item-id> | clear <order-id-or-item-id>",
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

fn supervise(arguments: &[OsString], implementer: Option<(&Path, &str)>, clock: &Clock) -> ! {
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
            Ok(Some(status)) => {
                if let Some((order_file, unit_name)) = implementer {
                    let signal = exit_signal(&status);
                    if let Err(error) = finalize_exited_implementer(
                        &compatible_command_paths().state,
                        order_file,
                        unit_name,
                        status.code(),
                        signal,
                        clock,
                    ) {
                        eprintln!("{error}");
                        std::process::exit(1);
                    }
                }
                std::process::exit(status.code().unwrap_or(1));
            }
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

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;

    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

fn run_pass_worker(role: CliPassRole, supervisor_pid: u32, clock: Clock) -> ! {
    let signals = register_signals().unwrap_or_else(|error| {
        eprintln!("ostrom: could not install signal handlers: {error}");
        std::process::exit(1);
    });
    let claude_bin = environment::CLAUDE_BIN.value_os().map_or_else(
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
        clock,
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

fn run_implement_worker(
    work_order_file: PathBuf,
    unit_name: String,
    supervisor_pid: u32,
    clock: Clock,
) -> ! {
    let signals = register_signals().unwrap_or_else(|error| {
        eprintln!("ostrom: could not install signal handlers: {error}");
        std::process::exit(1);
    });
    let working_directory = env::current_dir().unwrap_or_else(|error| {
        eprintln!("ostrom implementer: could not resolve working directory: {error}");
        std::process::exit(1);
    });
    let plugin_root = environment::OSTROM_PLUGIN_ROOT
        .value_os()
        .or_else(|| environment::CLAUDE_PLUGIN_ROOT.value_os())
        .map_or_else(|| working_directory.join("plugins/ostrom"), PathBuf::from);
    let request = ImplementRequest {
        paths: compatible_command_paths(),
        working_directory,
        plugin_root,
        order_file: work_order_file,
        unit_name,
        signals,
        supervisor_pid: Some(supervisor_pid),
        clock,
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

fn run_dispatch_command(arguments: Vec<String>, clock: Clock) -> ! {
    let [order_file] = arguments.as_slice() else {
        eprintln!("usage: dispatch.sh <work-order-file>");
        std::process::exit(2);
    };
    let working_directory = env::current_dir().unwrap_or_else(|error| {
        eprintln!("ostrom dispatch: could not resolve working directory: {error}");
        std::process::exit(1);
    });
    let plugin_root = environment::OSTROM_PLUGIN_ROOT
        .value_os()
        .or_else(|| environment::CLAUDE_PLUGIN_ROOT.value_os())
        .map_or_else(|| working_directory.join("plugins/ostrom"), PathBuf::from);
    let request = DispatchRequest {
        paths: compatible_command_paths(),
        working_directory,
        plugin_root,
        order_file: PathBuf::from(order_file),
        clock,
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

fn run_select_work(arguments: Vec<String>, clock: Clock) -> ! {
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
        clock,
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

fn run_doctor_command(
    check: Option<String>,
    clock: &Clock,
) -> Result<(), Box<dyn std::error::Error>> {
    let render_environment = check.is_none();
    let cwd = env::current_dir()?;
    let plugin_root = environment::OSTROM_PLUGIN_ROOT
        .value_os()
        .or_else(|| environment::CLAUDE_PLUGIN_ROOT.value_os())
        .map_or_else(|| cwd.join("plugins/ostrom"), PathBuf::from);
    let options = DoctorOptions::from_environment_at(plugin_root, clock.epoch_seconds());
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
    if render_environment {
        for variable in ostrom_store::ENVIRONMENT_VARIABLES {
            let (is_set, resolved) = variable.rendered_value();
            writeln!(
                io::stdout(),
                "ENV|{}|class={}|set={}|resolved={resolved}",
                variable.name,
                variable.class,
                if is_set { "yes" } else { "no" }
            )?;
        }
    }
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
    if environment::OSTROM_HOME
        .value_os()
        .is_some_and(|home| !home.to_string_lossy().trim().is_empty())
    {
        return resolved_or_exit();
    }
    // Empty means unset, matching the shell's `${CLAUDE_CONFIG_DIR:-...}`.
    // Without the filter an empty value resolves to the *relative* path
    // `ostrom/`, which reads whatever happens to be under the working
    // directory — the same defect `MANDATE_SECRETS_FILE` had. An explicit
    // non-empty value is honoured even if the directory does not exist yet,
    // because it is an instruction rather than a guess; the caller then gets a
    // named refusal that quotes it back.
    if let Some(config) = environment::CLAUDE_CONFIG_DIR
        .value_os()
        .filter(|value| !value.to_string_lossy().trim().is_empty())
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
    prepared: BTreeMap<String, PreparedCheck>,
    faults: BTreeMap<String, CheckFault>,
    catalogue_fault: Option<CheckFault>,
}

#[derive(Clone, Copy)]
enum CriteriaSelection {
    All,
    StaleOrNever,
}

struct CriteriaRunOutcome {
    passed: usize,
    failed: usize,
    inconclusive: usize,
    blocked: usize,
    faulted: usize,
    warnings: Vec<String>,
}

fn execute_prepared_checks(
    paths: &OstromPaths,
    resolutions: &PlanCheckResolutions,
    selection: CriteriaSelection,
    observed_at: DateTime<Utc>,
) -> Result<CriteriaRunOutcome, ostrom_core::CheckStoreFault> {
    let mut store = JsonlCheckStore::new(paths);
    let previous_runs = store.snapshot()?;
    let previous_receipts = previous_runs
        .iter()
        .flat_map(|run| &run.receipts)
        .cloned()
        .collect::<Vec<_>>();
    let run_id = new_check_run_id(observed_at);
    let receipts = resolutions
        .prepared
        .iter()
        .filter(|(_, prepared)| match selection {
            CriteriaSelection::All => true,
            CriteriaSelection::StaleOrNever => matches!(
                prepared
                    .resolved()
                    .evaluate(&previous_receipts, observed_at)
                    .state,
                CheckState::NeverRun | CheckState::Stale
            ),
        })
        .enumerate()
        .map(|(index, (id, prepared))| {
            prepared.execute_at(&format!("{}:{index}:{id}", run_id.0), observed_at)
        })
        .collect::<Vec<_>>();

    let passed = receipts
        .iter()
        .filter(|receipt| receipt.verdict == Some(CheckVerdict::Pass))
        .count();
    let failed = receipts
        .iter()
        .filter(|receipt| receipt.verdict == Some(CheckVerdict::Fail))
        .count();
    let inconclusive_receipts = receipts
        .iter()
        .filter(|receipt| receipt.verdict == Some(CheckVerdict::Inconclusive))
        .collect::<Vec<_>>();
    let inconclusive = inconclusive_receipts.len();
    let blocked = inconclusive_receipts
        .iter()
        .filter(|receipt| {
            resolutions
                .resolved
                .get(&receipt.check)
                .is_some_and(|check| check.inconclusive_policy == InconclusivePolicy::Block)
        })
        .count();
    let warnings = inconclusive_receipts
        .iter()
        .filter_map(|receipt| {
            let policy = resolutions
                .resolved
                .get(&receipt.check)?
                .inconclusive_policy;
            (policy != InconclusivePolicy::Block).then(|| {
                format!(
                    "check {} was inconclusive and allowed by inconclusive_policy: {}",
                    receipt.check,
                    match policy {
                        InconclusivePolicy::Block => "block",
                        InconclusivePolicy::Warn => "warn",
                        InconclusivePolicy::Pass => "pass",
                    }
                )
            })
        })
        .collect();
    let execution_faults = receipts
        .iter()
        .filter(|receipt| receipt.error.is_some())
        .count();
    let faulted = execution_faults + resolutions.faults.len();

    // A manual pass is recorded even when the catalogue selects nothing. An
    // automatic plan refresh stays quiet when every receipt is already fresh.
    if !receipts.is_empty() || matches!(selection, CriteriaSelection::All) {
        let completed_at = receipts
            .iter()
            .map(|receipt| receipt.completed_at)
            .max()
            .unwrap_or(observed_at);
        store.append_run(&CheckRun {
            schema_version: CHECK_STORE_SCHEMA_VERSION,
            run_id,
            completed_at: completed_at.to_rfc3339(),
            receipts,
        })?;
    }

    Ok(CriteriaRunOutcome {
        passed,
        failed,
        inconclusive,
        blocked,
        faulted,
        warnings,
    })
}

fn new_check_run_id(observed_at: DateTime<Utc>) -> CheckRunId {
    CheckRunId(format!(
        "criteria-{}-{:09}-{}",
        observed_at.timestamp(),
        observed_at.timestamp_subsec_nanos(),
        std::process::id()
    ))
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
            prepared: BTreeMap::new(),
            faults: BTreeMap::new(),
            catalogue_fault,
        });
    }

    let ids = enumeration
        .catalogues
        .iter()
        .flat_map(|catalogue| catalogue.document.checks.keys().cloned())
        .collect::<BTreeSet<_>>();
    let registry = ActionRegistry::core(plugin_root.to_owned(), cwd.to_owned())?;
    let mut resolved = BTreeMap::new();
    let mut prepared_checks = BTreeMap::new();
    let mut faults = BTreeMap::new();
    for id in ids {
        match registry.prepare(&id, &enumeration) {
            Ok(prepared) => {
                resolved.insert(id.clone(), prepared.resolved().clone());
                prepared_checks.insert(id, prepared);
            }
            Err(error) => {
                faults.insert(id, action_fault(&error));
            }
        }
    }
    Ok(PlanCheckResolutions {
        resolved,
        prepared: prepared_checks,
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

fn resolve_started_at(value: Option<&str>, clock: &Clock) -> Result<DateTime<Utc>, io::Error> {
    if let Some(value) = value {
        return parse_started_at(value, "--started-at");
    }
    Ok(clock.now())
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
    if let Some(path) = environment::OSTROM_LEGACY_HOME.value_os() {
        return Ok(PathBuf::from(path));
    }
    let base = BaseDirs::new().ok_or("could not resolve the legacy home directory")?;
    Ok(base.home_dir().join(".claude/ostrom"))
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{CheckCommand, Cli, Command};

    #[test]
    fn parses_criteria_run_check() {
        let parsed =
            Cli::try_parse_from(["ostrom", "check", "run"]).expect("parse criteria run check");

        assert!(matches!(
            parsed.command,
            Command::Check {
                command: CheckCommand::Run
            }
        ));
    }

    #[test]
    fn parses_skill_version_bump_check() {
        let parsed = Cli::try_parse_from([
            "ostrom",
            "check",
            "skill-version-bump",
            "--base",
            "base-sha",
            "--head",
            "head-sha",
        ])
        .expect("parse skill version check");

        assert!(matches!(
            parsed.command,
            Command::Check {
                command: CheckCommand::SkillVersionBump { base, head }
            } if base == "base-sha" && head == "head-sha"
        ));
    }

    #[test]
    fn parses_shell_retirement_check() {
        let parsed = Cli::try_parse_from(["ostrom", "check", "shell-retirement"])
            .expect("parse shell retirement check");

        assert!(matches!(
            parsed.command,
            Command::Check {
                command: CheckCommand::ShellRetirement
            }
        ));
    }

    #[test]
    fn parses_plugin_surface_check() {
        let parsed = Cli::try_parse_from(["ostrom", "check", "plugin-surface"])
            .expect("parse plugin surface check");

        assert!(matches!(
            parsed.command,
            Command::Check {
                command: CheckCommand::PluginSurface
            }
        ));
    }

    #[test]
    fn credential_requires_both_scope_halves_and_a_command_tail() {
        for arguments in [
            vec![
                "ostrom",
                "credential",
                "builder",
                "placeholder-org/alpha",
                "--permissions",
                "metadata:read",
                "--",
                "gh",
            ],
            vec![
                "ostrom",
                "credential",
                "builder",
                "placeholder-org/alpha",
                "--repositories",
                "placeholder-org/alpha",
                "--",
                "gh",
            ],
            vec![
                "ostrom",
                "credential",
                "builder",
                "placeholder-org/alpha",
                "--repositories",
                "placeholder-org/alpha",
                "--permissions",
                "metadata:read",
            ],
            vec![
                "ostrom",
                "credential",
                "builder",
                "placeholder-org/alpha",
                "--repositories",
                "placeholder-org/alpha",
                "--permissions",
                "metadata:read",
                "gh",
            ],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }

        let parsed = Cli::try_parse_from([
            "ostrom",
            "credential",
            "builder",
            "placeholder-org/alpha",
            "--repositories",
            "placeholder-org/alpha",
            "--permissions",
            "metadata:read",
            "--",
            "gh",
            "repo",
            "view",
        ])
        .expect("parse explicitly scoped credential command");
        assert!(matches!(parsed.command, Command::Credential { .. }));
    }
}
