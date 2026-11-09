# Ostrom direction

Status: draft, 2026-11-09, re-derived from [`experience.md`](experience.md). The experience comes
first: when the two disagree, `experience.md` wins and this document is changed. The pre-restart
Ostrom lives in Git history (see the restart PR for the archive tag).

## What Ostrom is

Ostrom is a super agent for one operator. Events come in from GitHub (later from chat, CI and
schedules). Ostrom decides whether each event asks for work, hands accepted work to an isolated
Worker session, routes later discussion to the Worker that already holds that thread, replies where
the event came from, and notices when a Worker stalls.

Every Ostrom session runs in the background. People reach Ostrom through GitHub, and the operator
reaches it through the `ostrom` CLI (and, later, a dashboard). Ostrom never creates T3 Code threads
or any other foreground session.

The design follows DeepSeek Harness's super-agent essay and its MIT prototype
(`deepseek-ai/dsh-superagent-prototype`, `docs/001-domain.md` and `docs/007-architecture.md`):

- A long-lived **Router** dispatches work and never executes it.
- **Workers** run one per task, in isolation, and never talk to people directly.
- **Triggers** pass deterministic filters first and a model second.
- **Thread subscription** sends follow-ups straight to the Worker that holds the thread.
- Sessions are full-duplex: they can be started, stopped and steered asynchronously.
- Compute, state and execution are kept separate.
- An external **Watchdog** handles retries and detects loops and drift.
- **Inline & Spill / Chain of Recall**: messages carry summaries plus ID pointers, and the reader
  fetches more on demand.

## Lessons so far

The first Ostrom built governance (signed policy, gates, actor manifests, its own runtime) before
any loop ran end to end. In W39 the loop was not installed and made zero unattended merges, and
about 3,000 lines were never used. **The rule for this restart: the loop runs end to end before any
governance, policy or gating feature is added.** Each such feature waits until a running loop shows
a failure that it would prevent.

The first milestone-1 build (#650–#657) ran the Router and Workers as T3 Code threads. That put
every background session in the operator's foreground workbench. The operator ruled on 2026-11-09
that T3 is for foreground sessions only. The T3-specific parts are replaced; the store, intake,
outbox, Router contract and Watchdog logic carry over.

## Ownership

| | Owns | Ostrom does not |
|---|---|---|
| **Ostrom** | Event intake and filtering, the Router, thread subscription and fan-out, the outbox (status cards, batching, reactions), commands, budgets, the Watchdog, driving Worker sessions through the vendor SDKs, Worker worktrees, the `ostrom` CLI, and one SQLite store | Store or display transcripts, provide a session viewer, or run an agent loop of its own |
| **Claude Agent SDK, Codex SDK** | The agent loop, tools, permissions and native session files of each Worker | — |
| **Semon** | Reading sessions: listing, transcripts, live state (`sessions --watch`, `--serve`), stalls, and one view across machines (`push`/`receive`) | — |
| **T3 Code** | The operator's foreground sessions. Ostrom does not cross it. | — |
| **GitHub** | Source of events and channel for replies | — |

Ostrom keeps no copy of transcripts. It stores each Worker's native session ID, harness, model and
machine. The operator reads a session with Semon, using the ID that `ostrom show` prints. Because
every Worker is a native Claude Code or Codex session, the operator can also open one directly
(`claude --resume`, `codex resume`) after `ostrom pause`, so that one session never has two drivers.

## How the pieces connect

```
GitHub ──poll──▶ ostromd ──filter──▶ store (SQLite) ──batch──▶ Router (one-shot SDK call,
  ▲                │  ▲                                          strict JSON: accept / ignore /
  │                │  └──────────────── decisions, reply text ◀── stop, reply wording)
  │                │
  │                ├─ accept ──▶ git worktree + SDK session (Claude or Codex) ──▶ opens PR
  │                ├─ follow-up on a subscribed thread ──▶ resume that session by ID, next turn
  │                ├─ commands (pause / resume / stop / extend) ──▶ session + store
  │                ├─ budgets: time, idle time, turns, tokens ──▶ pause at a limit
  │                └─ watchdog ◀── semon query stalls + session state ──▶ stuck ladder
  │
  └── outbox: status card edits, reactions, batched comments, one reply per review

ostrom (CLI) ──local API──▶ ostromd          Semon ──reads──▶ native session files
```

`ostromd` is the daemon and has no user-facing commands. `ostrom` is the operator's CLI and talks to
`ostromd` over a local API; the milestone-2 dashboard uses the same API.

The Router is a one-shot model call per batch, not a long-lived session. Every batch already carries
the whole contract, the undecided triggers with short excerpts and URLs, and the outcomes owed a
reply. So a new call needs no history, and there is no Router session to recover. The answer is one
strict JSON object; an answer that does not check out records nothing, and its candidates stay
candidates. The model makes the judgement calls and writes the words; the daemon does every
deterministic step.

A Worker runs in its own git worktree on a branch `ostrom/ticket-<ticket>-<assignment>`, so Semon
can attribute its session to the repository and branch. A follow-up on a subscribed thread goes from
the daemon straight to the Worker, as the next turn of the same session resumed by its ID, and never
reaches the Router. After an `ostromd` restart, running Workers are resumed by their stored IDs.

## Data model

Borrowed from the prototype's domain and narrowed to what milestone 1 needs:

| Entity | Meaning | Notes |
|---|---|---|
| `source` | GitHub, for one repository | Holds the read cursor, so a restart does not replay events |
| `trigger` | One immutable event (comment, review, PR state change) | Idempotent on the source event id; the actor is metadata |
| `thread` | An issue or a PR: Triggers in time order | Holds its subscriptions, its decision history and its status card's comment id |
| `decision` | The Router's judgement on a batch of Triggers: `accept`, `ignore` or `stop`, with a reason | Only an accept creates a Ticket |
| `ticket` | One unit of accepted work | `open → paused → open → ended`; carries its budget and what it has used |
| `assignment` | A Ticket bound to one Worker session | Harness, model, native session ID, worktree, branch and machine; `pending → running → ended`; a Ticket may have several |
| `subscription` | Ticket ↔ thread (many-to-many) | Fan-out without a new Decision; an accept subscribes its own thread, and the Worker's PR thread is subscribed automatically |
| `outbox` | Writes owed to GitHub: comments, card edits, reactions, labels | Batched per thread (10-minute window, at most one new comment per 30 minutes; blocking questions skip the wait); each sent once, retried with backoff |

## Milestone 1

Scope: [`experience.md`, "Milestone 1 scope"](experience.md#milestone-1-scope). One machine, the
private sandbox repository `onsager-ai/ostrom-sandbox`, and the GitHub experience complete.

Planned PRs, in order. Each is reviewed and merged on green CI before the next one starts, unless
noted.

1. **Session runner.** `src/session/`: one small interface over the Claude Agent SDK and the Codex
   SDK (start, send a turn, resume by ID, cancel, and per-turn usage), plus worktree creation. The
   SDK versions are pinned and covered by agreement tests. `src/t3/` and `vendor/t3code/` remain
   until PR 3, when their last callers move to the runner.
2. **Workers on the runner.** Dispatch, follow-ups as resumed turns, outcomes from turn results and
   PR state, and resume after an `ostromd` restart. A migration adds the session fields and
   `machine` to `assignment`.
3. **Router on the runner.** One-shot calls on a cheap tier with the existing strict JSON contract;
   the `router_thread` table goes.
4. **Status card and outbox.** One card per issue, edited in place; reactions as acknowledgements;
   batching; one reply per review. Snapshot tests of everything posted, and `ostrom preview`.
5. **Commands.** One command table in code for redirect, `@ostrom pause`/`resume`/`stop`, and
   closing the issue or PR, with permissions and reactions.
6. **Budgets and the stuck ladder.** Time, idle time, turns and tokens, with pause at a limit and
   `ostrom extend`. The Watchdog feeds the ladder (detect, recover, then the public stuck reply from
   the fixed template).
7. **The `ostrom` CLI.** `status`, `tickets`, `show`, `pause`/`resume`/`stop`, `extend` and
   `preview` over `ostromd`'s local API, all with `--json`. `ostromd` keeps only `run`; a systemd
   user unit runs it, with logs in journald.
8. **End to end on the sandbox.** Run the pass criterion below and record the evidence.

**Pass criterion:** on `ostrom-sandbox`, an issue becomes a merged PR, review follow-up included,
with no human relaying anything between GitHub, the Router and the Worker. Along the way the status
card stays a single comment; a `@ostrom pause` and `resume` keep the same session; an `ostromd`
restart mid-ticket resumes the Worker's session by its ID; and a scripted stall ends in the stuck
reply. The evidence is the store's records, joined to the native session IDs (read through Semon)
and the GitHub URLs.

Stack: TypeScript on Node, chosen by the user on 2026-11-09. No orchestration framework: the
operator ruled on 2026-11-09 that Mastra is not needed, and ACP is deprioritised. CI runs the tests;
nothing is built or tested locally on the operator's laptop. Workers run on cheap tiers
(gpt-6-luna, Haiku) unless a ticket's budget says otherwise.

## Non-goals (for now)

- Signed policy, gates, actor manifests, check catalogues, or verdict records.
- A session store, a transcript viewer or a log viewer. Semon reads sessions.
- Creating T3 Code threads, or "promote to foreground". Deferred by the operator on 2026-11-09:
  Workers are native sessions the operator can already open after `ostrom pause`.
- More than one repository or machine, chat sources, CI-as-source, webhooks or any publicly
  reachable listener, or a hosted service. The dashboard waits for milestone 2.

## Open questions

1. **Logins — still open.** On 2026-11-09, tiny SDK calls succeeded both as configured and with
   gateway environment credentials removed. Claude reported `apiKeySource: none` in both cases;
   that value also covers OAuth and other providers, so it does not establish the credential used.
   Codex's native records reported `magpie` as configured and `openai` with the SDK override; both
   calls completed, but neither the events nor the session record identified the credential source.
   The direct-provider calls therefore do not yet prove that the operator's logins were used.
2. **Model checks.** The Codex SDK reports token usage but not the model. Ostrom must confirm the
   model each turn used (from the native session record) so that a resume never silently runs on an
   expensive default, as happened once in the spike.
3. **Bot identity.** Replies go out through the operator's own `gh` login, so the "drop my own
   events" filter cannot rely on the actor. Option A: a hidden marker in each reply body (free, used
   in milestone 1). Option B: a dedicated bot account or GitHub App.
4. **Who merges.** Does the pass criterion allow a human to click merge, or must merging be
   unattended? A human merge is a decision, not relaying, but the target needs stating.
5. **Mid-turn crashes.** The spikes resumed sessions after crashes between turns only. What a Worker
   sees after `ostromd` dies mid-turn is still to be tested in PR 2.
6. **Semon readiness.** Today Semon exposes `list_sessions`, `get_session`, `read_transcript` and
   `stalls`. The Watchdog uses `stalls` through `semon query` (CLI, not MCP), matched to Workers by
   branch; see `docs/watchdog.md`. If the operator needs a ticket view in Semon, it is added there.
