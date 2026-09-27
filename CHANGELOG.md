# Changelog

## Unreleased

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
  "Constitution only; keep digest."
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
