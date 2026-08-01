---
name: doctor
description: Diagnose whether ostrom is actually wired up on this machine —
  run when the user types /doctor, or asks "is ostrom wired up", "why
  aren't my rules loading", "why didn't my touch save", or otherwise wants
  to verify/diagnose onboarding instead of assuming it worked.
---

# Doctor

A half-onboarded machine does not fail — it degrades silently. This skill
converts that silence into a short, actionable report.

## 1. Run the prober

Run `scripts/run-node.sh` (sibling of `skills/`, i.e.
`../../scripts/run-node.sh` from this file). The shim locates Node even
when an interactive version manager has not put it on `PATH`, then runs
the bundled doctor. The doctor is read-only (it makes exactly one write of
its own: a `git fetch` into the marketplace's cached clone, touching that
clone's remote-tracking refs only) and always exits 0; each line is:

```
STATUS|check-name|detail|remedy
```

`rule-distribution` always reports the number of `^## ` rules in the
installed payload. When an ostrom checkout is locatable, it also compares
that count, the full rule content, and the constitution plugin version
against the checkout. No checkout is a normal marketplace installation,
so that case stays `OK` with only the installed count.

## 2. Resolve any DEFER first

`provider-reachable` may come back `DEFER` for the `notion` provider —
the shell genuinely cannot see which MCP connectors are live for *this*
session, only you can. Resolve it yourself before rendering anything:

- Notion MCP tools available to you right now → treat it as `OK`,
  `✓ provider-reachable — notion MCP available to this session`.
- Not available → treat it as `FAIL`,
  `✗ provider-reachable — notion MCP not available to this session`,
  remedy `connect a Notion MCP server, or switch the touch provider to file`.

Never render a raw `DEFER` to the user — it is an internal handoff signal,
not a status.

## 3. Render

One line per check, in the order it printed them (with DEFER already
resolved per step 2), each with a marker:

- `OK` → `✓`
- `WARN` → `!`
- `FAIL` → `✗`

Format: `✓ check-name — detail` (omit the detail only if it is empty).

Then, only for lines that are not `OK`, a **Remedies** list: one bullet
per non-OK check, `check-name: remedy`.

Then a one-line verdict: fully wired if every check is `OK`; otherwise
name the worst status present (`FAIL` outranks `WARN`) and how many
checks carry it.

## 4. Keep it short, keep it clean

If every check is `OK`, keep the whole report to a few lines — the eight
check lines plus the verdict, nothing more.

Never echo a secret, token, or the Notion data source id — the prober
itself never prints them, and nothing above requires reading the
resolved config file yourself. Report provider name and durability only,
never touch-log *content*.
