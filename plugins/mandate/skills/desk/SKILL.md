---
name: desk
description: Read, lint, and decide mandate queue items. Use when the user types
  /desk, asks what portfolio decisions are waiting, or asks to approve,
  reject, defer, or lint a mandate.
argument-hint: "[list] | lint | approve <repo#number> | reject <repo#number> | defer <repo#number>"
---

# Mandate Desk

Show the durable pointer queue and apply only the decision the user makes.
Approval is the sole path that emits a handoff instruction.

## 1. Resolve config (layered YAML)

Read and merge these layers, most-specific wins:

1. shipped defaults — `config/defaults.yaml` in this plugin (sibling of `skills/`)
2. user — `~/.claude/ostrom/mandates.yaml`
3. repo — `./.ostrom/mandates.yaml` (if present)

The v1 provider must resolve to `file`. Its fixed private records are
`~/.claude/ostrom/queue.jsonl` and `~/.claude/ostrom/state.json`. Never
create, display, or commit a roster anywhere else. If no mandates file is
configured, say so and stop. Project scope is expressed with qualified glob
selector lists; unmatched work follows the explicit project `default`. A
`paused: true` project suppresses routine proposals, but reserved refs,
tripwires, and CI drift remain active.

## 2. List pending records

Run:

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/queue.sh" list
jq '.repos[] | {notice, unclassified, scope_changes}' \
  "${CLAUDE_CONFIG_DIR:-$HOME/.claude}/ostrom/state.json"
```

Each JSON row is a pointer with `id`, `repo`, `ref`, `title`, `kind`,
`mandate`, `state`, and `opened`, plus sweep facts when available: `age_days`,
`aged_out`, `needs_judgment`, and `blocked_by`. Present pending and deferred items
in this order: tripwire/decision, moved, stuck, drift. Keep the resolvable
`owner/repo#number` and its title visible. For a tripwire, include all four
fields from `mandate.dossier`: Question, Options ruled out, Recommended
action, and Blast radius. Do not fetch or copy an issue or PR body into the
queue.

If no action was supplied, stop after the list and ask for approve, reject,
or defer only when records are present.

## 3. Lint selectors on request

When the user runs `/desk lint`, run:

```sh
bash "${CLAUDE_PLUGIN_ROOT}/scripts/queue.sh" lint
```

Present every selector that matched no open item in the last durable sweep.
This is an on-demand config-quality diagnostic, not proof that a selector is
invalid or authorization to change the private roster. Never include these
diagnostics in the daily digest.

## 4. Apply exactly one decision

Resolve `<id>` from the displayed record; do not guess across ambiguous
references.

- **Approve** — run `queue.sh approve <id>`. This flips the row to
  `approved` and emits the instruction for the existing `/handoff` to Codex,
  including the minted `mandate:<id>` approval token. Relay that handoff
  instruction; never invent a broader token. CI drift from a paused project
  cannot be approved; unpause the mandate first.
- **Reject** — run `queue.sh reject <id>`. This removes the row and appends
  one line to `selector-events.jsonl`, attributing the dismissal to the
  selector that produced the row (an unmatched item still records that
  fact). This is bookkeeping on the decision already made, not a new step —
  never ask an extra question or add a prompt for it. Do not call
  `/handoff`, comment on GitHub, close the referenced item, or cause any
  other side effect.
- **Defer** — run `queue.sh defer <id>`. This keeps the row and flips its
  state to `deferred`. Do not call `/handoff`.

A tripwire never auto-proceeds. Only an explicit human approval may cross
it. The dossier shape is the constitution plugin's frozen escalation
protocol; this dependency points from mandate to constitution only.

State notices are digest rollups, not approvable queue items. For a mandate
change, use `scope_changes.entered` and `scope_changes.left` to show `/desk`
detail. An unclassified count asks for roster triage; it does not authorize
agent action.

## 5. Confirm

Confirm with the resulting queue record and, for approval only, the emitted
handoff instruction. No commentary.
