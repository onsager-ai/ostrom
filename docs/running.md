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

The operator uses `ostrom status`, `tickets`, `show`, `pause`, `resume`, `stop`,
`extend`, and `preview`. They use the daemon's Unix socket at
`$XDG_RUNTIME_DIR/ostrom/ostromd.sock`, or
`~/.local/state/ostrom/ostromd.sock` when `XDG_RUNTIME_DIR` is not set. The
read-only commands can inspect `OSTROM_DB` directly while the daemon is down;
mutating controls require the daemon.

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
Every `gh` invocation and every Git HTTPS credential request mints a fresh installation token, so
turns longer than an hour keep working. The configured repository's common SSH URLs are rewritten
to HTTPS in that process environment. Git credential requests are restricted to that repository;
ambient Git helpers are cleared. No token is written to the helper files. Worker commits inherit
both author and committer as the resolved App bot. Helpers are removed when the command/daemon
finishes, after its Workers drain.

This mechanism requires Workers to preserve PATH and Git's inherited environment and use normal
`gh`/Git commands. An absolute `gh` path, overridden credentials, `git -c credential.helper=...`,
or another SSH URL can bypass it; it is identity plumbing for trusted Workers, not a sandbox.
Each command starts with a fresh token, but a single Git/gh transfer that lasts longer than the
token lifetime can fail and must be retried explicitly. Minting on demand also costs API calls and
requires the key file and the Ostrom checkout to remain available throughout the turn. Live proof
of an App-authored sandbox PR, operator approval and App merge remains to be run by the operator.
