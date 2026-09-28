# Changelog

## Unreleased

- **Allowance routing: a declared runner order, reroute on a usage limit, never a silent wait (#626).** The manifest gains `defaults.implementers` (an ordered list of `{runner, model?, effort?}`, one entry per runner), a per-loop `implementers` override and `defaults.runner_retry`; undeclared, the order is `[agent/codex]`. `ostrom dispatch` resolves its runner from the current policy version (a loop's order through the new `OSTROM_LOOP`, which a loop-bound pass sets on its harness) and accepts `--runner <key>`. A runner's allowance refusal now ends the run with `reason: "runner-unavailable"` (was `codex-unavailable` for Codex's usage-limit message) and `runner`, marks it unavailable in `<state>/runner-availability.json` (new private state) until its reported reset or the declared retry, and is excluded from the repeated-failure escalation. A dispatch that skips a preferred runner records `work-rerouted` (new trace kind, mirrored as `work.rerouted`); when every runner is unavailable it emits a `decision.requested` and exits 3. The Claude harness gains an implementer mode in umwelt: sandboxed shell, writes confined to the worktree, network denied, Claude Code 2.1.259 or later. Because Claude Code silently runs without a settings file it cannot read, the harness validates the written profile's exact shape before launch (`implementer_profile_invalid` refuses it), and ostrom uses Claude only after a fail-closed sandbox canary passed for the installed `claude --version` and profile sha256 (cached in `<state>/sandbox-canary.json`, new private state; recorded as the new trace kind `sandbox-checked`, mirrored as `sandbox.checked`, whose `cost_usd` counts toward the daily cap). A failed canary marks the runner unavailable with `reason: "sandbox-unverified"`; a DENIED counts only when an unsandboxed `curl` control, under the operator's unchanged proxy environment, reached the same host first, and a control that fails makes it `sandbox-inconclusive` and caches nothing. `sandbox-checked` records the proxy variables with credentials removed. `runner-availability.json` entries carry `reason`. The Codex implementer passes a declared model and effort. **Breaking, library only:** `run_dispatch_with_registry` no longer takes a runner name; `DispatchRequest` gains `implementers`, `runner_override`, `runner_retry_seconds`; `ImplementRequest` gains `model`, `effort`, `runner_retry_seconds`; `PassRequest` gains `loop_name`; `umwelt_runtime::ImplementerRunRequest` gains `model`, `effort` and `max_turns`; `AgentRunner` gains a defaulted `installed_version`.
- **Resource admission: hold a launch the machine cannot take, refuse loudly when blind (#628).** O5 of #616. Optional `defaults.admission` (`max_cpu_temp_c`, `max_load_per_cpu`, `unit: {cpu_quota, nice, memory_max}`), overridable per loop field by field like `defaults.loop`'s `wall`/`idle`. `ostrom dispatch` checks it before an implementer launch — ahead of every GitHub call and worktree write, so a held attempt spends nothing finding that out — and `ostrom up` checks it before launching a loop worker. Reading the host (the hottest labelled `Package id 0` hwmon sensor, else the `x86_pkg_temp` thermal zone, else the maximum over every hwmon/thermal temp; `/proc/loadavg`'s 1-minute figure divided by the online CPU count) is `umwelt_runtime::read_host_resources`, with both roots (`/sys`, `/proc`) injectable via `MANDATE_ADMISSION_SYS_ROOT`/`MANDATE_ADMISSION_PROC_ROOT`; judging the reading against declared limits is `ostrom_core::AdmissionLimits::decide`, which never touches a filesystem. An over-limit reading records `admission-held` (`reason: "over-limit"`) and holds the attempt without starting anything or touching a lease; the same attempt is retried on the next dispatch or reconciliation. A declared limit whose sensor cannot be read holds just as loudly with `reason: "sensor-unreadable"` and a non-zero exit, rather than silently admitting. An undeclared limit skips the read entirely, so a solo operator on a machine with no sensors pays nothing for this (principle 2). `ostrom up`'s loop-run state gains the `admission-held` status, excluded from the "unchanged" settle check the way `inconclusive` already is, so a held slot is re-tried every reconciliation rather than waiting for the loop's own cadence; its summary line gains `admission-held=`. A rendered unit (loop or implementer) carries the declared `CPUQuota=`/`Nice=`/`MemoryMax=` when `defaults.admission.unit` names them. **Breaking, struct literals only:** `ostrom_store::DispatchRequest` gains `admission_limits`; `ostrom_core::{ManifestDefaults, LoopDecl, ResolvedLoop}` gain `admission`; `umwelt_runtime::LoopUnitDeclaration` gains `unit_resources`.
- **Stopping a run stops its harness child too (#633).** Fix-forward to #619, extended by item S2 of the #632 review. Codex (under an implementer) and Claude (under a pass) lead process groups of their own, which no signal to the run's own process reaches. When the stall reaper killed an implementer whose worker had not stopped Codex, or a pass whose worker had not stopped Claude (the pass lease names the worker, which does not lead a group), the harness kept running unwatched; on the process backend, in the item worktree after the lease was released. The worker now records its harness child as soon as it is spawned, in `<state>/harness/<supervisor pid>-<supervisor start time>.json` (new private state: `run_id`, `pid`, `process_group_id`, `process_start_time`), and removes it once the harness has exited. The supervisor stops a recorded harness that outlives its worker, before it records the run or releases its lease, and stops it too when a signal it forwarded has not stopped it within twice the termination grace. The reaper stops it once the run's own process is confirmed gone, including when it completes a claim a dead reaper left, and a harness it cannot confirm gone leaves the stop unconfirmed. Once the harness itself is gone, the rest of its process group (a build it started, say) is stopped too, and a stop is confirmed only when the group has no running member; the group is signalled only while the harness's pid is free or still its own zombie, so a reused id is never signalled. The record is private (0600), and a worker with no supervisor names it by its own pid. Every signal is identity-checked (pid, start time, process group), so a recycled pid is never signalled; nothing is signalled by pattern. The systemd backend already stopped the harness through the unit's control group and is unchanged. **Breaking, struct literals only:** `umwelt_runtime::ImplementerRunRequest` gains `spawned`, a `SpawnObserver` told the harness child's pid once it is spawned.
- **A suspend is not a stall (#637).** Fix-forward to #619, item S5 of the #632 review. A run's own watchdog, and systemd's unit timeouts, measure with the monotonic clock, which stops while the machine is suspended; the stall reaper measured with the wall clock, so after a laptop resumed the first `ostrom up` would have reaped every live run at its full cost ceiling. The reaper now measures a hold's gap since its last progress net of the time the machine spent suspended (`CLOCK_BOOTTIME` minus `CLOCK_MONOTONIC`, read with `clock_gettime` through `rustix` once per reaper run), and judges the expiry of a lease that names no process the same way. A pass acquiring the pass lease judges the previous holder's expiry net of suspended time too, so a pass that slept holding its lease is not taken over for that alone. Each reaper run records the suspended total in `<state>/reaping/suspend-timeline.json` (new private state), and a gap is bounded by the records on either side of it, so a missing record counts more time as suspended, never less. When the suspended time cannot be read (only a Linux kernel has `CLOCK_BOOTTIME`, so never on macOS or Windows), the reaper judges and reaps nothing that run, and records why in `last-error.json` for doctor; doctor judges no hold stalled either. `ostrom ps --json` gains `suspended_seconds`, and its `seconds_without_progress` is net of it. Rendered loop units carry `TimeoutStartSec=<wall + 120 s>` (`LOOP_UNIT_TIMEOUT_MARGIN_SECONDS`; 1920 when undeclared, was 1805), the same margin #635 gave implementer units, so the pass's own `pass-ended` wins the race with systemd's signal. The sweep lease is still judged by the wall clock alone. **Breaking, library only:** `ResolvedRunCaps::outer_bound_seconds` is replaced by `loop_unit_timeout_seconds`, `HoldProgress` gains `suspended_seconds`, and `StallError` gains `SuspendUnknown`.
- **A lease takeover is not an exit, and a pass that lost its lease never spawns (#636).** Fix-forward to #619. The stall reaper judges a pass by the generation of the lease it started under, not by whether some lease still names its owner. A pass whose lease the next pass took over (after a pause past the TTL, say) but whose recorded process still runs is displaced: the reaper records and charges nothing for it, and the pass ends itself with `pass-lease-lost`. A displaced pass past its stall threshold is stalled: the reaper stops the process its `pass-started` recorded and records it as it does any stalled pass. Only a pass whose recorded process is gone is closed with `exited-without-terminal`. A claim a dead reaper left on such a pass now stops its recorded process instead of taking the missing lease as a stop. The pass checks its lease after sweep preparation and immediately before it starts its harness, reading the lease file itself (owner and process) rather than only the renewal's flag, and ends with `outcome: "failed"`, `reason: "pass-lease-lost"`, `cost_usd: 0.0` instead of starting the harness alongside the new holder. Record shapes: `pass-started` gains `pid`, `process_group_id` and `process_start_time`, the pass's own process under the lease's field names. A `pass-started` written before this names none, and such a pass is closed as soon as no lease names it, as before.
- **The stall reaper stops before it records, records each reaped run once, and never stops scheduling (#635).** Fix-forward to #619. Reaping is now claim, stop, confirm, record: the reaper claims a run by creating `<state>/reaping/<run_id>.claim` exclusively, so of two racing reapers exactly one stops, records and charges it; it writes a terminal row only after a confirmed stop and only when the run wrote none, and a stop it cannot confirm (unreadable `/proc`, a unit state `systemctl` will not report, a process that outlived `SIGKILL`) writes nothing, releases nothing and keeps the claim for the next reaper, which also completes a claim whose reaper died. A run that handles the reaper's `SIGTERM` writes the claim's reason and charge into its own row, so the row says `stalled` whoever writes it. A reaper error is printed and recorded in `<state>/reaping/last-error.json` instead of failing `ostrom up` or `ostrom dispatch`, and doctor's `work-orders` check fails on that record and on every kept claim. `ostrom dispatch` reaps only the implementer holds for its own repositories (an unscoped, hand-run dispatch reaps nothing), and the reaper never signals its own process or process group. The claim records `signalled_at` before the first signal: only a signalled claim relabels the run's own row, and a process-backend supervisor finalizing a worker that died without its row honours it too; a claim kept without a signal leaves a later failure, such as a wall cap, its own reason. An empty claim or one naming a live pid with no start time is never taken for live, and is stale after two minutes; a claim is removed or rewritten only by the process that wrote it; and a reap clears only the recorded failures it examined. The lease mutation guard now names its holder and a guard left by a dead process is taken over, so pass-lease renewal no longer retries it silently until the lease expires; persistent contention is printed. A systemd implementer unit's `RuntimeMaxSec` is the wall cap plus 120 s (`IMPLEMENTER_UNIT_RUNTIME_MARGIN_SECONDS`; was plus 5 s), so the implementer's own `wall-cap` row wins. `docs/loops.md` documents that an implementer's idle cap counts transcript writes only. Record shapes: `<state>/reaping/*.claim` and `last-error.json` are new private state; a run-written `work-failed` or `pass-ended` for a reaped run gains `cost_basis` (and `last_progress_at`, `stalled_seconds`; the implementer's also `reaped: true`). **Breaking, library only:** `ostrom_store::reap_stalled_holds` takes a caller name and a repository scope and returns the reaped holds instead of a `Result`; `ReapedHold::reason` is a `String`.

- **Breaking: a loop that refuses an empty effective scope records one `loop-skipped` fact, not a `pass-started`/`pass-ended` pair (#613).** The refusal in `dispatch_resolved_loop` (`crates/ostrom-cli/src/main.rs`) sits ahead of the pass-versus-`cmd/run` branch, so it applies to every loop, including a `builder`/`gatekeeper` loop whose operation has an `agent/` step: no pass worker is ever spawned, so the old pair described a run that never started. `record_empty_loop_scope` now writes a single `loop-skipped` row carrying `owner`, `loop`, `actor`, a new `operation` field (`resolved.operation`), `repositories: []`, `skipped_repositories`, and `reason: "no-effective-repositories"` — no `outcome`, `cost_usd`, or `duration_seconds`, since nothing ran. `event_store::trace_event_type` gains `"loop-skipped" => "loop.skipped"`. A reader counting `pass-*` rows to detect these refusals sees fewer of them; `ostrom doctor`'s pass-completeness checks match by owner prefix (`builder-`/`gatekeeper-`), and these rows' owner is `loop-…`, so it never counted them and needs no change. The unbound pass path (`refuse_empty_repository_scope` in `ostrom-store/src/pass.rs`, a real pass process that did start) is unchanged and keeps its pair.
- **Every run is bounded, and a stalled hold is reaped (#619).** Loops and `defaults.loop` accept `wall` and `idle` (durations such as `90m`), and `defaults.implementer_ceilings` sets the implementer's. Undeclared, a pass or loop run gets a 30-minute wall cap and an implementer 4 hours; neither gets an idle default. The implementer now enforces its wall cap with a `CapsWatchdog` (terminal reason `wall-cap`), its systemd unit carries `RuntimeMaxSec=<wall + 5 s>` instead of `infinity`, and rendered loop units carry `TimeoutStartSec=<wall + 5 s>` (1805 when undeclared, was 1800). `ostrom up` and `ostrom dispatch` reap a live hold with no progress past its threshold, writing `work-failed` / `pass-ended` with `reason: "stalled"` and `cost_usd` equal to the declared cost ceiling (a pass that declared none: the shared per-run default `DEFAULT_RUN_COST_CEILING_USD`, 20 USD, which is also the work-order default; `cost_basis` names which), and close a pass whose process is gone with `exited-without-terminal`. The pass lease names its process, renews every 30 s and defaults to a 120 s TTL (was 3600 s). `ostrom up` records `skipped:previous-live` instead of launching over a live worker. `ostrom ps` and `ostrom doctor` show the caps; doctor's `work-orders` check fails on a stalled hold. Record shapes: `work-dispatched` gains `wall_seconds` and `idle_seconds`; a reaper-written `pass-ended` gains `recorded_by`. **Breaking, struct literals only:** `ostrom_store::ImplementRequest` gains `caps`, `DispatchRequest` gains `implementer_caps`, and `umwelt_runtime::LoopUnitDeclaration` gains `timeout_start_seconds`.
- **Breaking:** `ostrom hook session-start` is removed, with the layered
  constitution/rules injection behind it (#617): `render_constitution` and
  its helpers (`collect_layer`, `has_content`) in
  `ostrom-store/src/hooks.rs`, the compiled-in `SHIPPED_RULES` constant, and
  `assets/rules/frozen-rules.md` (the only file under `assets/rules/`, so
  the directory goes too) along with the rule-capitalization trigger it
  documented. `ostrom hook digest` and `ostrom local-drift` are unchanged —
  `render_digest`, `DigestOptions`, `HookOutput`, `decision_inbox_url`, and
  everything the digest reads (waiting decisions, escalated dispatch
  failures, undispatchable repositories, stalled holds, local drift) stay
  exactly as they were. The #617 plan originally grouped the digest with
  the constitution subsystem; the principal narrowed that ruling to
  "Constitution only; keep digest." `ostrom doctor`'s `environment` check
  goes with the constitution code it existed to diagnose: `check_environment`
  and `rule_layer_has_content` (`ostrom-checks/src/doctor.rs`) warned a
  cloud session that no user rules layer was resolved, a warning about a
  feature this PR deletes. Nothing else read the `CLAUDE_CODE_REMOTE`
  local-vs-cloud distinction, so the check is removed rather than
  narrowed, and `environment` drops from `DOCTOR_CHECKS`.
- **Breaking:** `ostrom explain` and `ostrom generate` are removed (#617). Both
  were operator introspection tools, not on the delivery loop's path:
  `PolicyBundle::explain_pull_request` has its own production caller in the
  sweep's policy holds (`sweep.rs`'s `update_policy_holds`), and that method,
  `compose`, `sign`, `validate`, and `rollback` are unaffected. `run_explain`,
  `run_generate`, their `ExplainOptions`/`ExplainTarget` types, GitHub
  pull-request acquisition (`acquire_pull_request`, `fixture_pull_request`),
  explanation rendering, and the repository-policy projection helpers
  (`project_repository_manifest`, `project_rules`) go with them, along with
  the now-unused `PolicyLoadError` variants they raised.
- `ostrom sweep --inner-org` no longer has an exit-code-6 path for a truncated branch or pull-request head-branch listing, and the outer sweep worker no longer translates a status-6 exit into `SweepError::BranchListingTruncated` (#579). Both were already unreachable: `acquire_repositories_independently` turns every per-repository error, `BranchListingTruncated` included, into a fault before the inner-org call can return it or the worker can exit non-zero for that reason, so no live behaviour changes. The variant survives — `fetch_branches` and `fetch_pull_request_heads_with` still refuse a repository whose branch listing hit its query limit rather than let `resolve_pull_request_heads`'s #562 join run on a partial list — but its two messages now read "refusing this repository's acquisition" instead of "refusing a truncated sweep", matching the per-repository outcome the code has always produced. A grep of this workspace found no other reader of exit code 6; whether a repository outside this workspace parses that exit code from `ostrom sweep` is still open and is called out in the PR for the coordinator to check before merge.

- **Breaking:** a disarmed pass now records `run.finished` with
  `outcome: unstarted`, `reason: disarmed` (#589). It previously recorded
  `outcome: no-op`, the same outcome a contended lease records, so a consumer
  could not tell a disarmed loop from an ordinary lease race by outcome
  alone. The lease-held path is unchanged: it still records `no-op` with
  `reason: lease-held`, and the process exit code for a disarmed pass is
  unchanged at 78. A consumer keying on `run.finished.outcome == "no-op"` to
  recognise a disarmed pass must switch to `outcome == "unstarted"` with
  `reason == "disarmed"`.
- **Breaking:** `ostrom audit` is removed (#617). It queried merged pull
  requests and joined them against recorded gate verdicts, but nothing in the
  delivery loop read its output — the gate, the sweep and `decision_answers.rs`
  read excuses and gate records directly. `AuditOptions`, `AuditError`, and the
  rendering internals that built the report go with it; `grant_excuse`,
  `revoke_excuse`, `list_excuses`, `active_exception_reason`, and `local_drift`
  are unaffected.
- **Breaking:** `ostrom replay` is removed, along with its `ostrom-store`
  module and CLI wiring (#617 C1). Nothing in the surviving delivery loop
  reads its output; it scanned merged pull requests for misses against the
  bounce selectors, a solo-operator report `ostrom queue lint` and `ostrom
  queue reject` do not replace. The dead `acquire_gate_replay_snapshot` and
  `evaluate_gate_replay` in `ostrom-store::gate`, re-exported at the crate
  root with no caller anywhere in the workspace, go with it — the live gate
  path already runs `acquire_metadata`, `evaluate_conditions`, and
  `aggregate` directly. Also fixed: the README described two commands that
  do not exist, `ostrom migrate` and `ostrom brief`, and `ostrom-cli`
  carried orphaned doc comments for them; `umwelt/README.md` claimed nothing
  depends on it, though `ostrom-cli`, `ostrom-store`, and `ostrom-checks`
  all link it.
- **Breaking:** `RepairError::Invalidate` is removed from the public
  `ostrom-store` enum (#599). A repair that changes a repository no longer
  fails outright when it cannot invalidate the sweep generation — contention
  with an in-flight sweep, or any other invalidation failure, is recorded as
  a stderr diagnostic instead, so the repair's own exit code is unaffected.
  Nothing outside this workspace consumes `ostrom-store` today, so this costs
  nothing now, but it is a breaking change to a public type and is recorded
  as one.
- Holdings: one run id per hold, recorded, passed to every harness child, and shown by `ostrom ps` (#618). `ostrom dispatch` now mints the implementer's run id and records it in `work-dispatched` as `run_id`, with `runner` (the registry key) and, when the dispatcher runs under an ostrom run, `parent_run_id`; it hands the id to `ostrom implement` through a hidden `--run-id`, and the implementer writes its events and its `work-completed` / `work-failed` row under it. `pass-started` gains `run_id`; `pass-ended` is unchanged. The record changes are additive. A pass's harness receives `OSTROM_RUN_ID`, and a dispatched implementer and its Codex child receive `OSTROM_RUN_ID` and `OSTROM_WORK_ORDER_ID`; both names are an external contract, documented in `docs/loops.md`. `ostrom ps` lists every open hold after the loop table (run id, runner, item, age, last event, lease state), and `ostrom ps --json` prints only the holds as JSON lines without needing a current policy version. `ostrom implement` sets both variables directly on its Codex child from its own effective run id and order, overriding anything inherited, so a hand run inside another run is never labelled with that run's id; otherwise a hand run behaves as before. **Breaking** for struct-literal construction only: `ostrom_store::ImplementRequest` gains a public `run_id` field (pass `None` for today's behaviour), and the vendored umwelt's `ImplementerRunRequest` gains a public `environment` field (pass an empty `Vec` for none).

## 0.16.0 (2026-09-18)

- `ostrom goals validate [<path>]` parses and validates an operator-authored
  goals document without running a pass (#602). Given a path it validates that
  file; omitted, it uses the discovery `ostrom plan` already applies — a
  per-repository override at `.ostrom/goals.yaml`, else the operator's
  `goals.yaml` in the config root — and both callers now share one definition of
  where goals live rather than two that can drift. Finding no document is a
  refusal, deliberately unlike `ostrom plan`, which treats an absent document as
  a legitimate empty plan; a test pins both so the stricter contract cannot leak
  into the pass. The exit status carries the verdict: 0 valid, 66 (`EX_NOINPUT`)
  could not be read or was not found, 3 unparseable YAML, 4 unsupported
  `goals_version`, 5 parses but semantically invalid. Those are refusal classes
  rather than one code per error, and 2 is never a document verdict: it is the
  most overloaded status in the binary, claimed by argument parsing before any
  command runs and by several commands' own failures.
- A `plan` loop preset, so `ostrom plan` can be scheduled through signed policy
  (#603). It declares the `planner` actor, the `portfolio-plan` operation
  wrapping `ostrom plan` as a `cmd/run` step, the `plan-portfolio` grant, and the
  `daily-plan` loop at `06:30` with `concurrent: 1` and `spend_usd: 5`. The
  preset needs the `gatekeeper` credential, but only when the pass actually
  sweeps: `run_plan` reuses a generation newer than `sweep.max_age` and mints no
  token in that branch, so a hand-run shortly after a successful sweep can pass
  without the secret while the scheduled pass fails. Declaring is not enabling —
  the loop ships unenabled pending #378.

## 0.15.0 (2026-09-15)

- Implementer dispatch gains a `process` backend, selected explicitly with
  `MANDATE_DISPATCH_BACKEND=process`, for environments without a user service
  manager (#590, #594). The implementer runs in its own session and process
  group and outlives the dispatching pass. It starts with a cleared environment
  plus an explicit allowlist, including proxy variables when set, and writes a
  private per-item log that is removed when its worktree is reclaimed. Its
  lease records process identity, and liveness is read from procfs alone: a
  dead, zombie or recycled implementer is reclaimed, and an unreadable process
  entry falls back to the lease TTL rather than being treated as dead.
  Implementers still inherit the dispatcher's descriptors that lack
  close-on-exec (#595).
- Loop-bound passes take `--loop <name>` and resolve their effective repository
  set from the verified manifest. The operator may supply the available set
  with `OSTROM_AVAILABLE_REPOSITORIES`; without it, the set is the repositories
  the policy and mandates name. Sweep always covers the whole available set.
  Work selection, dispatch and `ostrom gate` refuse repositories outside the
  effective set (#591).
- Loop wakes now fail before work when their effective repository set is empty,
  while gatekeeper wakes with a non-empty scope and no snapshot candidates end
  successfully without starting an agent session.
- Sweeps now hold an exclusive, 120-second lease across acquisition and
  generation writes, renew it every 30 seconds, and verify ownership before
  committing a generation. A killed holder expires within the short TTL, or
  is reclaimed immediately when procfs identifies it as dead. Passes wait up
  to 30 seconds for an in-flight sweep, and gatekeeper passes repair mismatched
  state and snapshot generations with one new sweep.
- Subset sweeps retain the prior generation's completion time for full-roster
  freshness, and a repository-changing pull-request repair invalidates the
  current generation.
- Worktree sweeps now remove the corresponding implementer log after removing
  an orphan or expired worktree, while retaining logs for retained worktrees.
- Every shipped prompt that invokes `ostrom gate` now requests its declared
  acquisition scope, with a test preventing any prompt from drifting.
- **Breaking:** loop declarations replace `target` with the scalar-or-list
  `repositories` field. Absent or empty means every available repository;
  manifests that still contain `target` are refused with the replacement name.
- **Breaking:** the shipped sweep preset no longer declares `loops.sweep`.
  Builder and gatekeeper passes refresh stale sweep generations before an agent
  session, and the shipped prompts no longer invoke a sweep themselves.
- **Breaking:** `ostrom loop run` (and the units `ostrom up` generates) now runs
  a builder or gatekeeper loop whose operation has an `agent/` step as a full
  loop-bound pass (`__pass-worker --loop`) instead of a bare agent run. Such a
  loop is now subject to the pass arming check, the pass lease and sweep
  freshness: a scheduled loop whose state has no `loop-armed` marker exits 78
  and records a failure on every slot after upgrading. Arm it before upgrading.
- A loop-bound gatekeeper judges only the pull requests in its pass's sweep
  generation, filtered to the effective repository set, instead of enumerating
  live open pull requests. A pull request opened after that generation waits
  for the next fresh generation, up to `sweep.max_age` (30 minutes by default).
- `ostrom plan` reuses a fresh sweep generation instead of always sweeping, and
  reports `swept` and `generation_id`.
- A manual `ostrom sweep` exits non-zero while another sweep holds the sweep
  lease (#599 tracks a distinct, retryable status).
