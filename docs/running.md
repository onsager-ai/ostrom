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
A lost response records unknown origin even when GitHub names the shared operator login.

This implementation blocks code-owner selection, last-push approval, merge queues and unsupported
active rules rather than guessing. A suite with more than 100 matching PRs blocks; its nested list
is not paginated yet. Required checks found only on GitHub's test-merge commit also block because
this reader evaluates the head rollup only. Unreadable or ambiguous protection responses block.
The sandbox matrix in `merging.md` remains a separate, unperformed verification step.

Set `OSTROM_WORKER_RULES_FILE` to a file of the machine's house rules (for example its heat
limits, and that tests run in CI rather than locally). Its text closes every Worker's brief.
