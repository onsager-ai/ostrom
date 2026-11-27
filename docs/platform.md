# Ostrom Management Platform

Status: accepted direction, 2026-11-27.

## 1. Purpose and positioning

T3 Code is already good at session execution, worktrees, and local agent orchestration. Ostrom does not rebuild T3 Code or mirror its thread UI. Instead, Ostrom is the **management platform above T3 and other runtimes**, turning `team-policy.md` and the Chief PM's manual coordination loop into durable, queryable, enforceable platform state:

1. **Decision Inbox (top priority):** One structured inbox for owner and Chief PM decisions, carrying evidence, a recommendation, options, authority tier, and urgency. Once answered, each decision is immutable and addressable by ID so Managers verify authorization directly instead of relying on quoted chat text.
2. **Lanes, Goals, and Weekly Plans:** Every workstream has a one-sentence goal, one number (current and target), an owner-approved weekly plan batch linked to its approval `decision_id`, and attached agents.
3. **Policy Engine:** Encodes `team-policy.md` rules—starting in `observe` mode (flagging breaches and logging friction): two-failure stop, authority routing (`owner` vs `chief_pm`), PR green-CI requirement, host limits, and registered automation only.
4. **Budgets and Spend:** Per-lane and team-wide caps and alerts backed by Semon (and later Magpie), keeping list estimates, source-reported USD, and billed USD distinct (`null` means unknown/unpriced, never `0`).
5. **Health, Stalls, and Friction (absorbing Sentinel):** Cross-host connector health, stalled or waiting runs (`needs_input`, `approval`, silent turns, repeated failures), and the daily adaptive friction log tracking owner interventions per day.
6. **Owner Digest:** Deterministic 01:00Z and 13:00Z digests plus weekly review summaries generated directly from platform state.

Under this management core sit pluggable **connectors**—starting read-only with **T3 Code** (M0/M2), followed by **GitHub** and **Semon** (M3), then **Magpie**, **Kiro**, **Antigravity**, and **Codex** (M4+).

## 2. Architecture

```
            ┌──────────────────────────────────────────────────────────┐
            │            Ostrom Management Core (on t3-sg)             │
            │  Decision Inbox · Lanes & Goals · Policy & Friction      │
            │  Budgets & Spend · Health & Stalls · 01:00Z/13:00Z Digest│
            │        SQLite WAL · Unix-Socket API · CLI · Loopback UI  │
            └───────────────────────────▲──────────────────────────────┘
                                        │ normalized facts (read-only first)
   ┌──────────────┬───────────────┬─────┴─────────┬──────────────┬──────────────┐
   │ T3 Connector │ Semon Adapter │ GitHub App    │ Magpie / Kiro│ Antigravity /│
   │ (/mcp, M0)   │ (ASI spend,M3)│ (PR/CI, M3)   │ (post-M3)    │ Codex (later)│
   └──────────────┴───────────────┴───────────────┴──────────────┴──────────────┘
```

### How T3 Code fits as an element
- T3 Code is one runtime connector (`src/connectors/t3/`) feeding normalized `agent`, `run`, `waiting_input`/`waiting_approval`, and `pr.watch_wake` facts from `t3-sg`, `hp-code-1`, and `hp-code-2` (all verified on T3 `v0.1.13`).
- T3's `(environmentId, threadId, runId)` stays strictly separate from Ostrom's `agent.id` / `run.id` and Semon's `session_uid`. A long-lived T3 thread can switch native sessions across runs, so Ostrom never equates a T3 `threadId` with a provider native session ID.
- Host `/mcp` origins remain strictly configuration-driven (never hardcoded in the repository). Infra Manager verified credential-free reachability and TLS from `t3-sg` (`t3` user) to existing HTTPS endpoints for `hp-code-1` and `hp-code-2` (port 443) and local loopback `http://127.0.0.1:3773/mcp`—requiring zero Tailscale Serve or T3 Connect changes. Node TLS + read-only OAuth remain to be exercised once owner approvals are granted at M1/M2.
- The T3 connector restricts calls at compile time and runtime to read-only MCP tools (`t3_environment_read`, `t3_project_list`, `t3_thread_list`, `t3_thread_read`, `t3_pending_request` `list`/`read`, `list_thread_pull_requests`) plus outbound SEP-3415 `events/stream` and `events/poll`. Any write path (webhook alerts, `t3_thread_send`, launches) requires separate Chief PM approval.
- Because T3's in-memory `MilestoneHub` holds 1,000 events and resets on server restart, any disconnect, `Gap`, or `truncated: true` triggers a full per-host reconciliation pass from thread reads.

### Isolation from `ostromd` and host footprint
- Runs on **`t3-sg`** as a standalone user systemd unit (`ostrom-platform.service`, `Nice=10`, `MemoryMax=256M`, target `<60 MB` RSS, near-zero CPU).
- Uses Node `>=24.22.2` built-ins (`node:sqlite`, `node:http`, `node:crypto`, `fetch`), zero new runtime npm dependencies, and zero frontend build steps.
- Uses its own SQLite WAL database (`$XDG_STATE_HOME/ostrom/platform.sqlite`) and Unix socket (`$XDG_RUNTIME_DIR/ostrom/platform.sock`, mode `0600`), completely isolated from `ostromd` on `hp-code-2`, `xray`, `nginx`, and headless T3.
- Viewed via a loopback-only (`127.0.0.1`) web bridge over an SSH tunnel using the per-run 256-bit bearer-fragment and `Host`/`Origin` security model from [`docs/dashboard.md`](dashboard.md).

## 3. Core data model

Forward-only `STRICT` SQLite WAL migrations (`src/platform/migrations.ts`):

| Table | Role and key columns |
|---|---|
| `lane` | Workstream container: `id`, `slug` (unique), `name`, `status` (`active\|paused\|archived`), `owner_agent_id`, `priority_rank`, `created_at`, `updated_at`. |
| `goal` | Weekly lane contract (`team-policy.md` §2): `id`, `lane_id`, `statement` (one sentence), `metric_name`, `metric_unit`, `metric_current`, `metric_target`, `week_start`, `weekly_plan_json`, `approved_decision_id` (FK -> `decision`, nullable until approved), `updated_at`. |
| `decision` | Durable approval inbox (`team-policy.md` §1, §4): `id`, `lane_id` (nullable), `authority` (`owner\|chief_pm`), `category` (`spending\|credentials\|infra_network\|release\|direction\|policy_exception\|weekly_plan\|routine`), `urgency` (`incident\|blocking_today\|digest`), `title`, `evidence_json`, `recommendation`, `options_json`, `status` (`pending\|approved\|rejected\|superseded\|expired`), `requested_by`, `decided_by`, `answer_json`, `rationale`, `superseded_by_id`, `created_at`, `decided_at`. **Immutable once decided** (enforced in code and by SQLite `BEFORE UPDATE` trigger; answered rows may only transition to `superseded` with `superseded_by_id`). Accompanied by append-only `decision_event`. |
| `policy_rule` | Executable policy definitions (`team-policy.md` §3, §6): `id`, `code` (unique: `two_failure_stop`, `authority_routing`, `pr_green_ci`, `host_limits`, `registered_automation_only`), `source_ref`, `mode` (`observe\|gate`, defaults to `observe`), `params_json`, `enabled`, `updated_at`. Paired with `policy_event` and `friction_entry` (`kind`: `correction\|repeated_ask\|owner_intervention\|gate_block\|policy_break`, `status`: `open\|selected\|kept\|reverted`). |
| `budget` | Caps and warning thresholds: `id`, `scope_kind` (`team\|lane\|agent`), `scope_id`, `period` (`daily\|weekly\|monthly`), `cap_usd`, `warn_pct`, `spend_source` (`semon\|magpie`), `enabled`, `updated_at`. |
| `spend_snapshot` | External spend records keyed by Semon `session_uid` (never by T3 `thread_id`): `session_uid`, `source`, `input_tokens`, `output_tokens`, `cached_tokens`, `usd_est`, `source_reported_usd`, `billed_usd`, `coverage`, `snapshot_id`, `as_of`, `observed_at`. All monetary fields are nullable (`null` = unknown/unpriced, never `0`). |
| `agent` | Managed actor across runtimes: `id`, `role` (`chief_pm\|manager\|worker\|sentinel`), `lane_id` (nullable), `connector_kind` (`t3\|semon\|github\|magpie\|kiro\|antigravity\|codex`), `host_id`, `external_ref`, `label`, `status` (`active\|idle\|waiting\|offline\|unknown`), `last_seen_at`, `updated_at`. |
| `run` | Execution unit: `id`, `agent_id`, `lane_id` (nullable), `connector_kind`, `host_id`, `external_run_ref`, `status` (`running\|waiting_input\|waiting_approval\|completed\|failed\|interrupted\|cancelled\|stalled`), `model`, `started_at`, `completed_at`, `failure_count`, `waiting_detail_json`, `semon_session_uid` (nullable; resolved strictly on exact `(origin_namespace, native_harness, native_session_id)` via Semon `dim_session`, never guessed from repo/branch/time or double-attributed from parent sidecars), `updated_at`. |

## 4. Decision Inbox contract (M1 top priority)

The Decision Inbox eliminates informal relay errors where approvals live only in chat transcripts:

1. **File:** A Manager or Chief PM submits a decision (`ostrom-platform decision create` / `POST /v1/decisions`) specifying `authority`, `category`, `urgency`, `title`, `evidence`, `recommendation`, and `options`. Rules in `team-policy.md` §1 determine whether `authority` must be `owner` or `chief_pm`.
2. **Route:** `incident` and `blocking_today` owner decisions surface immediately for Chief PM escalation; `digest` decisions batch into the next 01:00Z / 13:00Z digest.
3. **Answer:** The authorized decider records the answer once (`ostrom-platform decision answer <id>` / `POST /v1/decisions/:id/decide`), stamping `decided_by`, `decided_at`, `answer`, and `rationale`. The SQLite trigger blocks any later edit to those fields.
4. **Verify:** Before executing gated work, any Manager or automation runs `ostrom-platform decision verify <id> --json` (or `GET /v1/decisions/:id`). Exit code `0` means `status === "approved"`; any other state (`pending`, `rejected`, `superseded`, `expired`, or not found) exits non-zero (Repository Rule 4: a refusal is not a success).

## 5. Spend adapter boundary (Semon PM contract)

Agreed against `onsager-ai/semon@4dd600fc/docs/design/asi-store.md`:
- Query only through supported Semon interfaces (`semon query schema` + `semon query sql` or local stdio MCP) against an authorized materialized ASI snapshot—never open private SQLite/DuckDB files directly.
- Read `fact_session` per qualified session or `fact_step` exclusive rows for windows; never sum both and never count copied rows.
- Verified materialized ASI data currently resides on `hp-code-2` (not `t3-sg`), and the Fly pilot is on hold. Therefore M1–M2 expose a read-only `SpendSource` interface returning `unavailable` until a scoped aggregate-only handoff from `hp-code-2` is approved (no general SQL/Viewer owner token or raw logs in Ostrom).

## 6. Milestones and verification

All typechecks (`npm run typecheck`) and tests (`npm test`) run in GitHub Actions CI only—never locally. Every milestone ships through a PR with green CI and an independent Worker diff review; stop and escalate after two CI failures.

- **M0 (2026-11-27):** Read-only T3 connector (`src/connectors/t3/`)—MCP client, SEP-3415 poll/stream, PKCE OAuth, `0600` token store, CI tests.
- **M1 (2026-11-28):** `docs/platform.md`, 7-entity SQLite store (`src/platform/store.ts`), Decision Inbox + Lane/Goal/Policy/Budget Unix-socket API (`src/platform/api.ts`) and CLI (`ostrom-platform`), CI tests. Deliver exact 3-host OAuth approval steps to Chief PM upon merge.
- **M2 (2026-11-29 — First Live Demo on `t3-sg`):** T3 connector supervisor (3 hosts + restart/gap reconciliation), Policy Observer (`two_failure_stop`, authority routing, friction log), Health/Stall checks (absorbing Sentinel), 01:00Z/13:00Z Digest generator, loopback + SSH-tunnel static UI, and `ostrom-platform.service`.
- **M3 (2026-11-30 – 12-01):** GitHub App connector (PR/CI failure counts), Semon Spend adapter (once scoped aggregate handoff is approved), budget caps/alerts, soak ahead of 12-02 review.
- **M4 (post-12-02, gated on Chief PM approval):** Write paths (webhook alerts, controlled sends, `gate` policy mode) plus Magpie, Kiro, Antigravity, and Codex connectors.
