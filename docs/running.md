# Running Ostrom

`ostromd` is a user service. Install the two commands from a checkout with `npm link`
(they run their TypeScript directly on Node 24). Put the runtime configuration in
`~/.config/ostrom/env` (including `OSTROM_REPO`, `OSTROM_ALLOWLIST`, `OSTROM_DB`,
worker, router, and Watchdog variables). It must also set `PATH`: systemd starts user services
with a minimal one, and the daemon and its Workers need `node`, `ostromd`, `git`, `gh`, `semon`,
`codex` and `claude`. Copy your shell's value, for example with
`echo "PATH=$PATH" >> ~/.config/ostrom/env`. Then install the checked-in unit:

```sh
mkdir -p ~/.config/systemd/user
cp contrib/systemd/ostromd.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now ostromd
```

The daemon runs `ostromd run --interval 60`, restarts after failures, and writes
logs to the user journal. Inspect it with `journalctl --user -u ostromd`.

The per-ticket time budget counts time spent in Worker turns, including completed turns and any
currently running turn. Time waiting between turns, such as for review or an operator answer, does
not consume it; the idle budget applies only while a turn is running. After a crash, a recovered
turn ends at its last recorded session activity (or its start if no later activity was observed),
so daemon downtime does not consume the time budget.

The operator uses `ostrom status`, `tickets`, `show`, `pause`, `resume`, `stop`,
`extend`, and `preview`. Run `ostrom dashboard` to open the read-only local dashboard while
ostromd is running; it prints a private loopback URL and stays attached until interrupted. The local operator API also serves `GET /v1/intake?limit=50` with recent
observed events, Router decisions and excluded newly opened issues. The CLI and dashboard clients use
the daemon's Unix socket at
`$XDG_RUNTIME_DIR/ostrom/ostromd.sock`, or
`~/.local/state/ostrom/ostromd.sock` when `XDG_RUNTIME_DIR` is not set. The
read-only commands can inspect `OSTROM_DB` directly while the daemon is down;
mutating controls require the daemon.

## Model discovery and selections

`ostrom models` lists the cached Claude and Codex catalogs, allowed selections, efforts,
custom settings and discovery errors. Add `--json` for structured output. Discovery runs in the
background at daemon startup using the same harness environment as Workers; `ostrom models
refresh` refreshes it on demand. A failed refresh exits nonzero and retains the last good catalog.
New models start disallowed; retired allowed models are flagged in the list.

```sh
ostrom models allow codex/gpt-6.1-sol
ostrom models set codex/gpt-6.1-sol --efforts low,high --approval on --cost 2 --note "Rust changes"
ostrom models remove codex/gpt-6.1-sol
ostrom models objective cost
ostrom models export > models.json
ostrom models import models.json
```

Use the IDs and efforts shown by discovery. Settings require an allowed model; efforts must be a
subset of its discovered efforts and cost must be a positive relative weight. Import replaces the
objective and all selections atomically after validating them against the local cached catalogs.
An unavailable model cannot be added or edited, but an existing retired selection can be removed.
Listing and export work from `OSTROM_DB` while the daemon is down; changes and refresh require it.
The Router chooses a model at acceptance using the allowed list, objective, cost and notes. Each
Worker turn gets a separate small Router call choosing an allowed effort from its prompt and recent
history. Invalid model answers fall back to the cheapest allowed model without approval; invalid
effort answers use medium (or an allowed default). Decisions and inputs appear in `ostrom show
<ticket> --json`; the status card and dashboard show the current turn and reasons. With no selected
models, `OSTROM_WORKER_MODEL` remains the session model and effort is still chosen per turn.

`ostrom model <ticket> <harness/model>` and `ostrom effort <ticket> <level>` set durable overrides
through the daemon. On the issue or PR, use `@ostrom use <harness/model>` and `@ostrom effort
<level>`; issue authors and collaborators with triage or higher may do this. Later effort overrides
apply at the next turn; an override in the comment that starts a ticket applies on its first turn and
is shown on the status card without a separate reply. A session keeps its model: a model override after session start applies only to the next
session. Overrides persist until replaced and must remain in the allowed list.

The existing merge approval mechanism does not cover starting a model. An automatic choice that
requires approval is recorded and falls back to the cheapest allowed model without approval. If
none exists, dispatch waits. An explicit model override approves that model for this ticket. This
approval does not grant permission to merge. Outcome history and escalation are later phases.

Developer-only one-shot passes remain available as `ostromd dev <pass>`:
`poll`, `send`, `dispatch <ticket>`, `observe`, `commands`, `route`, `watchdog`,
`merge`, `route-candidates`, and `tick`. They are retained for their existing tests and
for controlled sandbox diagnosis.

Merging is unattended by default. Set `OSTROM_MERGE_STRATEGY=human` to wait for an external
merge (by a person or another tool), or set `OSTROM_MERGE_STRATEGY=approvals` together with a positive
`OSTROM_MERGE_APPROVALS` and optional comma-separated `OSTROM_MERGE_APPROVERS`. Before an
unattended squash merge, Ostrom performs fresh GitHub evidence reads, requires the ready reply
actually to have posted, and waits ten quiet minutes. It sends only SHA-guarded squash merges;
it never uses bypass, auto-merge, another method, or branch deletion. GitHub changes can still
race the final request, so repositories requiring deliberate timing should use the human policy.

A first merge-enabled restart gives existing Worker PRs a new ready notice and quiet window;
previous green status does not make them immediately mergeable. The window restarts for stored
human issue/PR triggers, completed Worker turns, review submissions, review-thread resolution
changes, head/base changes, and policy changes. GitHub does not expose a reliable timestamp for
review-thread resolution, so observing that change restarts the window at observation time.
Direct comment/body edits also restart the window when observed; marked Ostrom replies/cards and
CI progress do not. Unobserved activity that is undone between reads and invisible pending review
drafts cannot be detected. Changing from human to automatic policy resets the window but updates
the card rather than posting another opened reply.

The final send refreshes intake/controls, issue state, comments, PR/check/review/thread evidence and
branch requirements. A failed prerequisite pass suppresses sends while reconciliation continues.
Only a confirmed GitHub merge ends work. Definite HTTP 4xx refusals back off (408 and 429 are treated
as ambiguous); transport errors and 5xx keep the request unresolved until a fresh GitHub read.
A lost response records unknown origin when GitHub names the shared operator login. With App
configuration, the resolved App bot's numeric user ID establishes Ostrom attribution.

This implementation blocks code-owner selection, last-push approval, merge queues and unsupported
active rules rather than guessing. A suite with more than 100 matching PRs blocks; its nested list
is not paginated yet. Required checks found only on GitHub's test-merge commit also block because
this reader evaluates the head rollup only. The classic-protection endpoint's HTTP 404 with exact
message `Branch not protected` means no classic protection; active rules and all other merge
requirements still apply. Other 404s, any 403 and all other protection-read errors block.
The sandbox matrix in `merging.md` remains a separate, unperformed verification step.

Set `OSTROM_WORKER_RULES_FILE` to a file of the machine's house rules (for example its heat
limits, and that tests run in CI rather than locally). Its text closes every Worker's brief.

## GitHub App identity

Set both `OSTROM_GITHUB_APP_ID=4544316` and `OSTROM_GITHUB_APP_KEY_FILE=/absolute/path/to/key.pem`
for the daemon and CLI. Keep the PEM readable only by the service user. Never put its contents or
an installation token in the environment file, repository, logs or brief. Setting just one variable
fails startup; setting neither retains ambient `gh` authentication. The App's name can change:
only its ID is configured. Startup looks up `GET /app` and `GET /users/{slug}[bot]` to resolve the
current bot author/committer name and noreply email.

Install the App on `OSTROM_REPO` (the current App is installed on the organization's repositories).
Ostrom discovers that repository's installation with an App JWT at
`GET /repos/{owner}/{repo}/installation`, then exchanges it for an installation token. The App
needs Contents, Issues and Pull requests write, and Actions, Checks and Commit statuses read.
**Administration: read is also needed for classic branch-protection reads.** Without it, automatic
merge may block on an inaccessible protection response. Installation/settings changes remain an
operator action. Reserve this App's credential for Ostrom so its bot ID proves merge attribution.

Daemon/CLI `gh` children receive `GH_TOKEN` in their environment, never argv; the daemon caches
it in memory until three minutes before expiry. JWTs use RS256 with 60 seconds of clock skew and
a ten-minute maximum claim window. Keys are read only to sign; keys and tokens are never logged
or saved. Authentication errors omit API bodies and credential values.

Workers inherit temporary executable helpers on PATH and Git's process-scoped credential settings.
Every wrapped `gh` invocation and every Git HTTPS credential request mints a fresh installation token, so
turns longer than an hour keep working. The configured repository's common SSH URLs are rewritten
to HTTPS in that process environment. Git credential requests are restricted to that repository;
ambient Git helpers are cleared. No token is written to the helper files. Worker commits inherit
both author and committer as the resolved App bot. Helpers are removed when the command/daemon
finishes, after its Workers drain. Workers also receive `GH_CONFIG_DIR` pointing at an empty,
private directory alongside the helpers, and ambient `GH_TOKEN`/`GITHUB_TOKEN` are removed.
The wrapper supplies a fresh `GH_TOKEN` to its own child; GitHub CLI gives that token precedence
over stored credentials (`gh help environment`). If a shell reorders PATH or uses an absolute
`gh` path, the unwrapped CLI has no saved login and fails authentication instead of acting as the
operator. This configuration isolation applies only to Workers; the daemon's runner is unchanged.
In App mode, Codex sessions disable login shells (`allow_login_shell=false`) and shell snapshots
(`features.shell_snapshot=false`) so login profiles cannot put the operator's `gh` ahead of the
helper. Non-App sessions retain their existing shell behavior; Claude shell behavior is unchanged.

This mechanism requires Workers to preserve PATH and Git's inherited environment and use normal
`gh`/Git commands. An absolute `gh` path, overridden credentials, `git -c credential.helper=...`,
or another SSH URL can bypass it; it is identity plumbing for trusted Workers, not a sandbox.
Each command starts with a fresh token, but a single Git/gh transfer that lasts longer than the
token lifetime can fail and must be retried explicitly. Minting on demand also costs API calls and
requires the key file and the Ostrom checkout to remain available throughout the turn. Live proof
of an App-authored sandbox PR, operator approval and App merge remains to be run by the operator.
