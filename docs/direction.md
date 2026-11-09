# Ostrom direction

Status: draft, 2026-11-09. This is a direction, not a spec. The pre-restart Ostrom lives in Git
history (see the restart PR for the archive tag).

## What Ostrom is

Ostrom is a super agent for one operator. Events come in from GitHub (later from chat, CI and
schedules). Ostrom decides whether each event asks for work, hands accepted work to an isolated
Worker session, routes later discussion to the Worker that already holds that thread, replies where
the event came from, and notices when a Worker stalls.

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

## Lesson from the first attempt

The first Ostrom built governance (signed policy, gates, actor manifests, its own runtime) before
any loop ran end to end. In W39 the loop was not installed and made zero unattended merges, and
about 3,000 lines were never used. **The rule for this restart: the loop runs end to end before any
governance, policy or gating feature is added.** Each such feature waits until a running loop shows
a failure that it would prevent.

## Ownership

| | Owns | Ostrom does not |
|---|---|---|
| **Ostrom** | Event intake and deterministic filtering, the Router's tools, thread subscription and fan-out, replies (an outbox), a small policy (who may start work), the Watchdog, and one SQLite store | Run processes, store sessions, or provide a UI |
| **T3 Code** | Worker and Router sessions: threads, worktrees, providers and models, steer/stop, and PR linking on threads | — |
| **Semon** | Recall and analytics over normalized sessions: transcripts, stalls, cost, and SQL/FTS through MCP/CLI | — |
| **GitHub** | Source of events and channel for replies | — |

Ostrom keeps no copy of transcripts. When it needs to recall a Worker's session, it stores the T3
thread id and asks Semon or T3 for the session.

## How the pieces connect

```
GitHub ──poll──▶ ostromd ──filter──▶ store (SQLite) ──batch───────▶ Router (T3 thread)
                   │  ▲                                              │ t3 thread send --wait:
                   │  └──── JSON: accept / ignore / stop, replies ◀──┘ one strict JSON answer
                   │
                   ├─ accept ──▶ t3: create Worker thread (worktree) ──▶ opens PR
                   ├─ follow-up on a subscribed thread ──▶ t3: send/steer to that Worker
                   ├─ observe ◀── t3 thread state (subscribeThread / show)
                   └─ watchdog (schedule) ◀── semon query stalls + t3 state ──▶ flag to Router
```

The daemon (`ostromd`) runs intake, filtering, fan-out, the outbox and the Watchdog, and it calls
T3 itself. `docs/t3-feasibility.md` shows that it can. The Router is a long-lived T3 Code thread
that judges candidates and words replies. Each pass (`ostromd route-candidates`), the daemon sends
it one self-contained batch with `t3 thread send --wait --json`: the contract, the undecided
triggers with a short excerpt of each body and its URL, and the outcomes owed a reply. The Router
answers with one strict JSON object; an answer that does not check out records nothing, and its
candidates stay candidates. The Router has no Ostrom MCP tools, because at the pinned T3 version
(`322f6544df`) a thread gets exactly one MCP server, T3's own `t3-code`: the Claude adapter builds
`mcpServers` with that entry only, the Codex adapter sets only `mcp_servers.t3-code`, and neither
the thread, project nor provider-instance contract has a field for another. The model makes the
judgement calls and writes the words; the daemon does every deterministic step, including ending a
ticket once a terminal outcome (PR merged or closed, Worker failed or cancelled) has its reply. A follow-up on a subscribed thread goes straight from the daemon to
its Worker and never reaches the Router.

## Data model

Borrowed from the prototype's domain and narrowed to what milestone 1 needs:

| Entity | Meaning | Notes |
|---|---|---|
| `source` | GitHub, for one repository | Holds the read cursor, so a restart does not replay events |
| `trigger` | One immutable event (comment, review, PR state change) | Idempotent on the source event id; the actor is metadata |
| `thread` | An issue or a PR: Triggers in time order | Holds its subscriptions and its decision history |
| `decision` | The Router's judgement on a batch of Triggers: `accept`, `ignore` or `stop`, with a reason | Only an accept creates a Ticket |
| `ticket` | One unit of accepted work | `open → ended`; a person's message resumes it |
| `assignment` | A Ticket bound to one T3 Worker thread id | `pending → running → ended`; a Ticket may have several |
| `subscription` | Ticket ↔ thread (many-to-many) | Fan-out without a new Decision; the Worker's PR thread is subscribed automatically |
| `outbox` | Replies owed to GitHub, each with an id | Each reply is sent once; retried with backoff |

## Milestone 1

Scope: one repository, with GitHub as the only source.

1. A person comments on an issue or PR. `ostromd` takes the Trigger in, drops its own comments and
   anyone not on the allowlist, and hands the candidate to the Router.
2. The Router accepts or ignores it. On accept, it acknowledges on GitHub through the outbox.
3. `ostromd` creates a Worker thread in a fresh worktree. The Worker opens a PR, and the PR's thread
   is subscribed to the Ticket.
4. Later review comments on that PR go to the same Worker, which pushes fixes.
5. The Router replies on GitHub with the outcome.
6. A scheduled Watchdog uses Semon's `stalls` read and T3 thread state to flag a stalled Worker to
   the Router.

**Pass criterion:** an event becomes a merged PR, review follow-up included, with no human relaying
anything between GitHub, the Router and the Worker. The evidence is the store's records, joined to
the T3 thread ids and the GitHub URLs. The one scripted stall must be flagged by the Watchdog.

Stack: TypeScript on Node, chosen by the user on 2026-11-09. It matches T3 Code's contract schemas
and the prototype. CI runs the tests; nothing is built or tested locally on the operator's laptop.

## Non-goals (for now)

- Signed policy, gates, actor manifests, check catalogues, or verdict records.
- A process runtime, a sandbox, or a session store. T3 Code and Semon already provide them.
- More than one repository, chat sources, CI-as-source, webhooks or any publicly reachable
  listener, a hosted service, or a UI panel.

## Open questions

1. **Follow-up delivery.** Should follow-ups use `t3 thread send` (queued as the next turn) or the
   WS `message.dispatch` with `steer`? Proposal: use the CLI first and switch when a queued
   follow-up turns out to be too late.
2. **How Workers and the Router report.** Option A: they get Ostrom MCP tools (`report`, `ack`,
   `handback`; for the Router `candidates`, `decide`, `reply`). Option B: `ostromd` derives
   Worker outcomes from T3 thread state (`status`, `settled`, `linkedPullRequest`,
   `pendingRequestCount`) and reads the Router's judgement from its run's reply. **Chosen: B for
   both in milestone 1.** At the pin T3 Code gives a thread no MCP server but its own (see "How
   the pieces connect"), so A would mean registering the server in config T3 does not own, per
   provider: user-level settings reach every session on the machine, and project-level settings
   reach every thread in the project. Revisit if T3 adds per-thread MCP servers.
3. **Bot identity.** Replies go out through the operator's own `gh` login, so the "drop my own events"
   filter cannot rely on the actor. Option A: a hidden marker in each reply body (free, used in
   milestone 1). Option B: a dedicated bot account or GitHub App.
4. **Who merges.** Does the pass criterion allow a human to click merge, or must merging be
   unattended? A human merge is a decision, not relaying, but the target needs stating.
5. **Router recovery.** If the Router thread's context fills up or the thread dies, a new Router has
   to be started from store state. Milestone 1 keeps it minimal: the store records each Router
   thread, every batch carries the whole contract, and a pass whose stored thread T3 reports gone
   (`t3 thread show`: "is no longer available") starts a new one with that batch. Detecting a
   full context or a stuck Router is still open; decide whether that is a Watchdog job.
6. **T3 version drift.** The WS contract is T3's internal one. Decide where the pin and the agreement
   test against `@t3tools/contracts` live. Settled for the CLI surface: the pin is `src/t3/pin.ts`
   (`t3` v0.1.7, release tag commit `322f6544df`), the upstream schemas are vendored under
   `vendor/t3code`, and `src/t3/agreement.test.ts` checks the parsers against them. The WS
   contract gets the same treatment when Ostrom first uses it.
7. **Semon readiness.** Today Semon exposes `list_sessions`, `get_session`, `read_transcript` and
   `stalls`. SQL/FTS is step 4 of Semon's ASI plan. Milestone 1 should need only `stalls` and
   `read_transcript`.
