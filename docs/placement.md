# Worker placement

Run Workers on a configured second host over SSH, through a thin `ostromd worker` entry point
that calls the existing SDK runners there. Keep one `ostromd run`, SQLite store, poller, Router,
outbox, merge pass and Watchdog on the daemon host. No new network listener or hosted service.

Status: proposed, 2026-11-16. This document specifies M3; none of the remote commands or variables
below exists yet. The dashboard follows placement.

## Why this comes next

The [first M2 run](e2e-m2.md) hit the hp-code-1 thermal guard three times at 90–91 °C when Workers
ran. The entire run moved to hp-code-2 to [finish M2](e2e-m2-hp2.md). Today a host unable to sustain
Workers cannot host Ostrom at all. M3 separates those loads; it does not promise a thermally weak
host can sustain the Router, which stays local.

M3 lifts only the Worker-machine part of the current single-machine restriction. One repository
and one daemon remain. The placement strategy in [experience.md](experience.md#machine-placement-strategy)
is the longer-term direction: its per-machine daemons, capability registration, automatic headroom
selection and `ostrom pin` are deferred. M3 configures one destination for new Workers; existing
assignments retain affinity. No model chooses a host, retries a transport or judges its health.

## Options and choice

| Option | Fit with the current implementation | Main cost |
|---|---|---|
| SSH to a thin `ostromd worker` using the SDKs on that host | Reuses `Runner`, `Session`, native IDs and permissions; SSH already carries stdio | A small remote command protocol and restart reconciliation |
| SSH directly to `codex exec` / `claude` | Runner-up; native sessions, no listener | Duplicates SDK event/result handling and cancellation for two CLIs |
| Point each SDK at a remote-executable wrapper | Executable overrides can launch a local SSH wrapper | CLI pipes alone do not move worktree creation, model verification, credentials or recovery; coupling leaks into both SDK adapters |
| T3 Code environments | Suitable for the operator's deliberate foreground work | Ostrom would need foreground threads; violates the ownership boundary |

Choose the first. Keep the SDK loop on the machine that owns its filesystem and harness login.
Do not invent a general remote execution service. Install the same Ostrom revision and pinned SDK
versions on both hosts; reject a protocol/revision mismatch before dispatch. Agreement tests cover
the narrow message shapes and SDK events we actually read.

## Dispatch and the boundary

The operator provisions a dedicated checkout of the one repository on the Worker host, the native
harness logins, Node, Git, `gh`, Semon and thermal tooling. No login files move from the daemon.
SSH uses an operator-configured alias, batch mode and existing host-key verification; Ostrom never
accepts a new host key or falls back to the local host after a failure.

The daemon invokes a fixed remote command, `ostromd worker`, with no brief, path or credential in
its shell command. Versioned JSON messages travel on stdin/stdout; stderr is diagnostic only and
must omit secrets. Requests identify the assignment by a daemon-generated UUID (the branch suffix),
not just store-local integer IDs. Operations are prepare, start-turn, attach/status, cancel and
acknowledge-result. Input paths are absolute paths on the Worker host, never shell fragments.

1. Persist the destination, UUID, branch, remote worktree path, harness/model/provider and initial
   turn request before sending. The first brief is durable too; it must survive losing the session
   ID response. Reuse the durable queued-turn delivery key for subsequent turn requests.
2. Prepare verifies the remote checkout's origin matches `OSTROM_REPO`, fetches the configured base
   and creates a worktree at the fetched base SHA with today's UUID branch/path naming. Return and
   store that SHA. Never share `.git` over a network filesystem or copy the daemon's dirty checkout.
   An identical prepare request returns the same worktree; conflicting parameters fail non-zero.
3. Send the brief, base, branch, model/provider, budget limits and house rules as data. The Worker
   host reads its rules file; append those rules to the ordinary brief. Do not forward the daemon's
   whole environment. Remote harness configuration comes from its own private environment file.
4. Start one background process for that turn, using the existing `Runner.start` or `resume`.
   Persist its native session ID remotely before emitting it; the daemon stores the ID with the
   assignment's machine and remote path. Session and transcript files stay on that host.
5. Return the existing `TurnResult` fields (text, status, usage, verified model) plus the request ID
   and structured failure when applicable. Codex rollout/model verification runs on the Worker
   host, rather than reading a daemon-local session directory. Preserve provider across resume
   (the failure recorded in issue #715); configuration changes must not silently switch it.
6. Record completion/usage and finish the durable turn once, then acknowledge the remote result.
   Lost acknowledgements return the same result without charging usage or sending replies twice.
   A result alone never authorizes merging; the daemon still runs every M2 prerequisite.

The remote process holds an OS lock for the assignment across the whole native turn. A private
remote directory retains only request identity/parameters, session ID, process identity, state and
unacknowledged result, written atomically. This is execution receipt data needed to reconcile SSH
loss, not a second ticket store or transcript archive. Keep acknowledged receipts through assignment
end to reject replay, then remove them with its execution metadata; worktree cleanup stays separate.
A repeated request attaches to the same process/result. Another request while that turn holds the
lock is refused. Confirm process identity with PID and start time; never kill by process name.

## Control and restart

The SSH attachment may die while the turn process keeps running. A second invocation can query
or attach to it. On daemon restart, reconcile remote receipts before today's `Workers.recover()`
logic marks an open turn interrupted. A running process is reattached; a completed result is
consumed once. Only a confirmed dead process with no final result permits the existing interrupted
recovery prompt, on the same host/session. A missing receipt for a request that may have run is
ambiguous: stop automatic delivery and ask the operator, rather than starting another session.
This also covers loss of the first session-ID response and remote host reboot.

Cancel targets the exact turn request, calls the SDK abort path, and waits for the native process
and tools to stop before recording cancellation and releasing the assignment lock. Killing an SSH
client is not confirmation. Pause/stop retain their current durable daemon intent and merge block;
queue the existing push-WIP/pause or draft-PR/stop cleanup turn only after cancellation is confirmed.
An unreachable host leaves cleanup pending, not a successful pause cleanup or terminal stop.
Resume and follow-ups go to the recorded host even if the configured destination has changed.
The operator opens `claude --resume` / `codex resume` on that host only after confirmed pause cleanup.

## GitHub App credentials

The private key stays on the daemon host. Remote mode requires App configuration; ambient remote
operator `gh` authentication is not a fallback. Keep the current App bot author/committer identity,
empty private `GH_CONFIG_DIR`, HTTPS repo rewrite, cleared ambient Git helpers/tokens and protected
helper PATH. Explicitly pass the App-mode shell settings without sending the key-file variable.

Adapt the existing Git credential and `gh` helpers to request a fresh installation token through
a private per-turn Unix socket on the Worker host. The attached `ostromd worker` relays those
requests over SSH to the daemon's existing App signer and returns tokens in memory. This local
socket connects tool subprocesses to the attachment; it exposes no network service. Restrict Git
requests to the configured repository as today. The daemon mints repository-scoped tokens; helpers
supply `GH_TOKEN` only to their `gh` child or answer Git's credential pipe, never argv or disk.

A single startup token is insufficient: long turns already need fresh tokens per Git/gh command.
With no daemon attachment, token requests fail within a bounded timeout; they never use an operator
login. Reattachment restores the relay for later commands, without replaying failed writes. Tokens
already supplied can remain usable until expiry, so transport loss is not credential revocation.
Workers remain trusted processes, not a sandbox against credential theft. Neither key material nor
tokens belong in briefs, receipts, result messages, transcripts or logs.

## Watchdog and failures

Query `semon query stalls --idle-minutes N --since 1d --json` on each assignment host over SSH,
once per host per pass. Semon still owns session reading; Ostrom stores only its existing stall
records. Match host plus exact branch and session ID, rather than branch alone across all hosts.
Preserve today's distinction between a running turn and a Worker waiting for review.

Semon `push`/`receive` already transports redacted logs and machine facts, and `sessions --machines
DIR` displays received machines. Focused queries support repeated `--machine DIR`; do not assume
that the viewer's `--machines` flag is the Watchdog query API. Received facts can be stale and require
a receiver/pairing service. Choose a live host-local query for M3. Operator-owned Semon sync remains
optional for viewing; Ostrom neither copies transcripts nor starts a receiver.

| Failure | Required behavior |
|---|---|
| Host unreachable before dispatch or mid-turn | Explicit transport error; preserve pending/running request and block merges/new turns for it. Never report completed or infer process death from SSH exit. Reconcile on the next normal pass; repeated failure uses the existing stuck path without overlapping recovery turns. |
| Daemon exits/restarts mid-turn | Keep remote receipt/process; reattach or consume the result before scheduling. While absent, no new token minting. Graceful shutdown requests cancellation; unconfirmed cancellation remains unresolved. |
| Remote process/host dies | Retain worktree/session; confirm lock is free and process is gone, then recover by ID. Missing/contradictory evidence stays blocked for operator action. |
| Semon fails, returns malformed data or host cannot be read | Fail Watchdog non-zero; never clear that host's stall episodes or merge its assignments using an empty result. Other hosts can still be observed. |
| Clocks disagree | Daemon owns budget, queue and merge timestamps; remote durations use a monotonic clock. Check clock offset with round-trip bounds before dispatch/reconnect; uncertainty or offset above 60 seconds blocks new turns and time-based Semon decisions until corrected. Never subtract remote wall time from daemon wall time. |
| Worker host overheats | Run thermal checks on that host before preparation/turns; wrap the turn process in `thermal gate`. At 85 °C or above, exit 75, stop its children and retain interrupted work; refuse recovery/new turns while hot. Missing supported sensor (69) is reported, not invented heat evidence. Never wait in a hot retry loop. |

A failed operation exits non-zero and records a concrete reason in the daemon's existing error/card
path. Clock or thermal failure does not stop intake/outbox on the daemon. Remote elapsed time and
usage must still count toward budgets; losing SSH must not reset a turn's budget clock. Send the
remaining wall/idle allowance at turn start and enforce it remotely with monotonic timers even
without an attachment; expiry cancels the turn and records budget pause pending daemon cleanup.

## Configuration (proposed)

Put the daemon values in the existing private `~/.config/ostrom/env`:

| Variable | Meaning |
|---|---|
| `OSTROM_WORKER_SSH` | Unset: current local execution. Set: one operator-managed SSH alias, e.g. `hp-code-2`. |
| `OSTROM_WORKER_REMOTE_ENV_FILE` | Required with SSH: absolute path to the Worker host's private environment file. |
| `OSTROM_WORKER_REPO`, `OSTROM_WORKER_WORKTREES` | Existing variables; paths on the selected host. |
| `OSTROM_WORKER_RULES_FILE` | Existing variable; read on the selected host. |
| `OSTROM_WORKER_BASE`, `OSTROM_WORKER_HARNESS`, `OSTROM_WORKER_MODEL` | Existing dispatch choices, sent explicitly. |

The remote env file supplies PATH and harness auth/provider settings, never the App key or daemon
DB. Remote execution metadata lives under that host's XDG state directory. Startup validates the
SSH-only combination, remote revision, repository, tools and App mode. Record the returned hostname
and configured connection alias with the assignment; changing an alias must not move an existing
session. Refuse a different returned host identity during recovery. Old local assignments remain
local even after SSH is enabled; disabling SSH cannot reinterpret remote paths as local paths.
With SSH unset, keep today's local runners, auth helpers, rules loading and configuration defaults.
No capacity registry, automatic failover, session migration, fleet scheduler or dashboard in M3.

## Live acceptance on the sandbox

After implementation and green CI, record an `e2e-m3.md` with revisions, both hostnames, sanitized
configuration, UTC timeline, temperatures, request/session IDs, store rows and GitHub links. Use a
fresh sandbox store and one healthy remote Worker host; never deliberately heat a machine.

1. Keep daemon/Router/store on host A and run a Codex Worker on B. Observe its remote worktree,
   native files and stored machine/ID; no Worker process or worktree appears on A. Open an App PR,
   receive a review follow-up in the same B session, pass PR CI, merge unattended on A and post Done.
2. Exercise remote Claude start/resume too. For each harness, pause, confirm WIP pushed and no
   driver remains, restart the daemon, then resume the same native ID on B. Stop another ticket:
   WIP pushed, PR draft, one summary, terminal state only after cleanup.
3. Interrupt only the SSH attachment mid-turn, then restart A while B still runs. Observe one native
   driver, recover its ID/result, count usage once and deliver a queued follow-up once. Separately
   kill the remote turn by its verified PID and prove same-session interrupted recovery.
4. Make B unavailable during a turn and request stop. Observe failed passes, visible pending
   cleanup and no merge/local fallback. Restore B and reconcile before cleanup or any new turn.
5. Observe a fresh token request after a token lifetime has elapsed, without printing the token;
   git/gh still act as the App. Disconnect the relay and prove credential requests fail closed.
6. Exercise a scripted remote stall: host-local Semon evidence joins the stored assignment, the
   existing ladder runs without duplicate drivers, and one stuck reply appears. Failed Semon reads
   retain the episode. Controlled injected skew and thermal-refusal results prove non-zero/blocking
   behavior; label these simulations separately from measured temperatures and live SSH failures.
7. With SSH unset, repeat a local issue-to-PR smoke run and confirm unchanged local placement.

M3 is done only when these observations are recorded; unit tests alone do not prove placement.

## Questions for the operator

- Which host should carry the daemon and which SSH alias should receive new Workers for the live
  run? Recommend A=hp-code-1, B=hp-code-2 if A can sustain the Router; confirm availability and logins.
- Is pausing unattended remote work until its original host returns acceptable after disk loss or
  a prolonged outage? Recommend it for M3; migration needs a separate recovery design and evidence.
