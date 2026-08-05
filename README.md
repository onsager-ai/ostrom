# ostrom

A Claude Code plugin marketplace shipping small mechanisms for
governing agent-workflow conventions and steering a portfolio of
projects.

The spine is Elinor Ostrom's work on governing a commons: rules carry
provenance (`Source:`) and falsifiable `Preconditions:`, a human
gatekeeps what actually gets frozen, and rules have a lifecycle that
includes retirement. The engine itself is deliberately content-free —
it ships the mechanism and two generic meta-rules, not somebody's
personal constitution.

## What it ships

- **`constitution` — layered SessionStart constitution injection** — shipped rules,
  then user rules, then repo rules (most-specific wins).
- **The rule-capitalization trigger** — the agent proposes freezing a
  rule after the same class of correction recurs; it never
  self-installs one.
- **Agent-push `/touch` intervention capture** — with pluggable log
  providers (file/Notion) and layered YAML config.
- **`mandate` — a daily portfolio sweep and SessionStart digest** — reads
  open GitHub issues, PRs, and CI through `gh`; keeps a private,
  file-backed queue of pointers; and routes approve/reject/defer decisions
  through `/desk`. Tripwires reuse constitution's escalation-dossier
  protocol and never auto-proceed.

## Layout

- `.claude-plugin/marketplace.json` — marketplace catalog (this repo is the marketplace)
- `plugins/constitution/` — the plugin: frozen rules (injected at SessionStart), /touch skill, /doctor skill
- `plugins/mandate/` — the independent portfolio plugin: daily sweep, SessionStart digest, private queue, and /desk skill
- `plugins/constitution/hooks/inject-constitution.sh` — SessionStart hook: emits the layered constitution (shipped → user → repo)
- `plugins/constitution/config/` — shipped defaults + reference examples for the /touch log (provider choice + layered YAML config) and for private rules (`rules.example.md`)
- `plugins/constitution/scripts/run-node.sh` — Node-resolution shim behind /doctor (including non-interactive nvm/fnm/volta/asdf environments)
- `plugins/constitution/tools/` — TypeScript source, tests, and build configuration for the /doctor prober
- `plugins/constitution/dist/doctor.js` — committed, zero-runtime-dependency /doctor bundle
- `repo-pointer/settings.json` — snippet to merge into each target repo's `.claude/settings.json`
- `bootstrap.sh` — one command to make a fresh environment ostrom-aware (user-level enroll + config provisioning)
- `LICENSE` — MIT

## Install

```
/plugin marketplace add onsager-ai/ostrom
/plugin install constitution@ostrom
/plugin install mandate@ostrom
```

Or the scripted path, from a clone of this repo:

```
./bootstrap.sh                 # user-level enroll (~/.claude/settings.json)
# then, once inside Claude Code:
/plugin install constitution@ostrom
```

`bootstrap.sh` enrolls at the **user level**, so every repo in that
environment picks up ostrom — no per-repo pointer needed for your own
sessions. It's idempotent and backs up an existing `settings.json`. It
also drops a **zero-secret default `/touch` config** at
`~/.claude/ostrom/config.yaml` (file provider), so logging works
immediately with no Notion account — see [Touch-log config](#touch-log-config).

Known caveat: a project's settings.json pointer registers the
marketplace but external-source plugins still need the one-time
install command per environment (claude-code issue #32606).

## Mandate

`mandate` resolves its small YAML schema in three layers: shipped defaults
→ `~/.claude/ostrom/mandates.yaml` → `./.ostrom/mandates.yaml`.
Copy `plugins/mandate/config/mandates.example.yaml` to the user path and
replace its placeholder roster. The real roster, queue, read cursors, and gate
exceptions remain machine-local at `~/.claude/ostrom/mandates.yaml`,
`~/.claude/ostrom/queue.jsonl`, `~/.claude/ostrom/state.json`, and
`~/.claude/ostrom/exceptions.jsonl`; none belongs in this repository.

`search_roots` is an opt-in list of local directories. The local drift scan
discovers Git repositories beneath each root, enumerates every linked worktree,
and reports dirty, unpublished, patch-equivalent landed, and fully pushed
branches with no open or merged PR. Shipped defaults leave the list empty, so
no local paths are guessed. Run `plugins/mandate/scripts/local-drift.sh` for
detail.

Each project uses case-insensitive qualified glob selectors in `delegated`,
`excluded`, and `bounce`; `reserved` is a list of exact issue/PR numbers.
Supported selectors are `label:`, `scope:`, `type:`, `path:`, `ref:`, and
`title:`. `*` is the only wildcard (`path:**` spans directory depth), and a
`title:` selector must include `*`. Pull requests inherit the labels and refs
of their closing issues.

Classification precedence is reserved → shared/project bounce → excluded →
delegated → `default`. The default is `unclassified`, which produces one
per-repo `/desk` triage line rather than one queue row per item; projects may
explicitly choose `default: delegated` or `default: excluded`. Pausing a
project suppresses routine work but never reserved refs, tripwires, or failing
CI. The first sweep baselines existing work, and selector changes re-baseline
scope rather than flooding the queue.

Run the read-only sweep daily outside Claude Code. For example, edit the
placeholder clone path and install this with `crontab -e`:

```cron
0 8 * * * cd /absolute/path/to/ostrom && CLAUDE_PLUGIN_ROOT=/absolute/path/to/ostrom/plugins/mandate /bin/bash /absolute/path/to/ostrom/plugins/mandate/scripts/sweep.sh
```

The SessionStart hook never calls `gh`; it renders the durable files written by
the scheduled sweep and performs only the local portion of the drift scan. It
emits one JSON document whose
`systemMessage` displays the digest to the operator and whose
`hookSpecificOutput.additionalContext` gives the assistant byte-identical
text. Empty sections disappear, so a healthy portfolio's digest text is
exactly `N projects nominal`; local drift adds one detail-free line only when
found. If the state file is older than `cadence_hours`,
the hook adds one short stale warning. Queue rows contain a
resolvable GitHub pointer, its sweep-refreshed title, and mandate metadata,
never mirrored issue or PR bodies. When an open PR closes a queued issue,
only the PR is shown. Digest rows preserve at least 45 title characters when
available, truncating only the non-selector tail of long reasons. Each issue
and PR query reads up to 200 open items; reaching that cap adds a persistent
per-repo incomplete-sweep warning. v1 implements the `file` provider only; the
provider seam remains explicit for a later addition.

Run `/desk lint` explicitly to inspect selectors that matched no open item in
the last sweep; unmatched selectors never add daily digest lines.
Baseline and mandate-change summaries render once, then are acknowledged in
the private state so they do not become permanent session noise.

### Sprint lease and trace

Builder wakes coordinate through a lease file at
`${CLAUDE_CONFIG_DIR:-$HOME/.claude}/ostrom/sprint.lease`. Run
`scripts/lease.sh acquire <owner> [ttl-seconds]` before starting work,
`scripts/lease.sh release <owner>` when finished, and `scripts/lease.sh status`
to inspect the current record. The file contains one JSON object with
`owner`, `started_at`, and `expires_at` (times are Unix seconds). Creating the
file with Bash `noclobber` uses O_EXCL semantics as the atomic acquisition
point, so concurrent builders cannot both win. A held lease cannot be replaced
before expiry, an expired lease can be reclaimed, and only its owner can
release it. Release and expiry replacement use a short-lived O_EXCL guard only
to serialize deletion; the guard contains no lease or trace data and is
removed before normal return.

Meaningful builder steps append to
`${CLAUDE_CONFIG_DIR:-$HOME/.claude}/ostrom/sprint.jsonl` with:

```json
{"ts":"2026-01-01T00:00:00Z","kind":"commit","fact":{"sha":"0123456789abcdef"},"narration":{"reason":"placeholder change"}}
```

Use `scripts/trace.sh append <kind> <fact-json> <narration-json>` to write a
record. `fact` holds actions, artifacts, identifiers, external results, and
exit codes; `narration` holds reasons, beliefs, and conclusions. A builder or
gatekeeper consuming another builder's trace uses `scripts/trace.sh read`,
which emits only `ts`, `kind`, and `fact`. Narration is for the principal and
requires the explicit `scripts/trace.sh read-narration` verb. This structural
split prevents one builder's narration from becoming another builder's input.

Each trace record, including its newline, is limited to 4096 bytes and is
appended by one shell `printf`, matching the queue JSONL discipline. Oversized
records are rejected instead of risking an interleaved append.

The lease and trace are machine-local runtime state. Like the real roster,
queue, and read cursors, they never belong in this repository.

### After implementation: the gatekeeper loop

The builder implements work, opens a pull request, and **stops**. It never
merges its own delivery. In a separate session, the gatekeeper polls every
repository in the mandate roster, evaluates each open pull request from its
current GitHub artifacts through `/merge`, and takes only the action the gate
permits: approve as the App and merge a pass, report a fail, or escalate an
inconclusive result to the principal. The gatekeeper does not write code,
suggest fixes, or review for quality. The builder's only response is a new
commit.

The principal starts and owns that separate gatekeeper session and answers its
escalations. The builder must not start it: whoever controls when review runs
can also decide when it does not. A different model from the builder is
recommended, not required; the separate role provides structural independence,
and a different model adds cognitive independence.

From a project where the `mandate` plugin is installed, the principal starts a
dedicated gatekeeper session with the same recurring wake mechanism as the
sprint loop:

```sh
claude --settings ~/.claude/ostrom/roles/gatekeeper.settings.json "/loop 30m /gatekeep"
```

The `--settings` flag is not decoration. It applies the gatekeeper profile from
[`docs/role-permission-boundaries.md`](docs/role-permission-boundaries.md),
which denies the writing capabilities this role must not hold — commit, push,
branch mutation — and denies review-thread resolution to both delivery roles.
Started without it, the session inherits the principal's default permissions,
which are builder-like: the separation then exists only in prose, and a session
able to commit is a session able to satisfy the conditions it is judging.
Create the profile before the first run.

Thirty to sixty minutes is the recommended polling period; the principal may
choose within that range, but the gatekeeper should not run faster than the
builder's sprint pass. Each wake covers all open pull requests and treats each
one independently. An inconclusive result escalates once for a given pull
request head SHA and then stays quiet until the artifact changes; repetition
never turns it into a pass.

The principal can resolve a failed or inconclusive condition for exactly one
pull request artifact with the
[one-PR exception path](docs/role-permission-boundaries.md#one-pr-exception-path).
The grant is an append-only event scoped to the current head SHA and one named
condition; a new commit makes it stale without deleting its audit record.

The cost is explicit: **nothing merges while no gatekeeper is running**. That
is the intended price of keeping merge authority out of the builder, not a
defect or a hidden background service. Claude Code's `/loop` is session-scoped,
so closing the gatekeeper session stops the polling.

### Selector accuracy

`/desk lint` reports selectors that matched nothing, which is config hygiene,
not accuracy. Two different errors matter and they are not symmetric. A **miss**
is a safety failure — something crossed a boundary unreviewed. A **false alarm**
costs an interruption. Prefer recall wherever an irreversible action is in
reach and accept the precision loss there; prefer precision everywhere else,
because an interruption budget spent on noise is unavailable when it matters.

Both are measured, and neither is reduced to a single score:

- **False alarms** accrue going forward. Rejecting an item in `/desk` appends one
  line to `~/.claude/ostrom/selector-events.jsonl` recording which selector put
  it in front of you. Nothing extra is asked at decision time.
- **Misses** are computed retroactively. `scripts/replay.sh` is read-only: it
  scans merged pull requests for changes touching an irreversible surface —
  workflow files, release tooling, credential-shaped paths — that matched no
  bounce selector. Its output is a **lower bound**, not the miss rate: a change
  that touched nothing on that list and matched nothing may still have been a
  miss.

The report is a table with one row per selector, and it names each prefix's
tier, because they are not equally trustworthy:

| Tier | Prefixes | Derived from |
|---|---|---|
| Content-derived | `path:`, `ref:` | the change itself |
| Author-written | `title:`, `type:`, `scope:`, `label:` | text the item's author chose |

`type:` and `scope:` are parsed out of the conventional-commit prefix of the
item's **title**, and labels are set by whoever opened the item. So for any gate
resting on the author-written tier, the party being gated selects whether the
gate fires — a release pull request titled `chore: bump version` silently misses
`type:release`. `path:` is also pull-request-only, so issues have no
content-derived gating at all.

Prefer content-derived prefixes and exact `reserved` refs wherever a condition
carries real safety weight. A single accuracy number would hide precisely this
split, which is why the report does not produce one.

## Cloud / CI

No token, no credential setup — the marketplace is public, so the
`git clone` `/plugin marketplace add` does under the hood needs no
auth. Run `bootstrap.sh` in the environment setup script, then, once
per persistent environment, inside Claude Code:

```
/plugin install constitution@ostrom
```

A **private fork** of this marketplace would need git credentials for
the clone (`/plugin marketplace add` fetches a private source with
`git clone`, not the API) — the only remaining reason anyone would
care about that path.

## Per-repo enrollment

Only needed for **teammates or shared repos** — your own sessions are
covered by the user-level `bootstrap.sh` enrollment above. To make a
repo self-register for anyone who opens it, merge
`repo-pointer/settings.json` into its `.claude/settings.json` and commit.

## Rules layering

The SessionStart injection is **layered**, most-specific wins: shipped
`frozen-rules.md` → `~/.claude/ostrom/rules.md` + `rules.d/*.md`
(user) → `./.ostrom/rules.md` + `rules.d/*.md` (repo). A later layer wins on
conflict; a missing layer is skipped silently, so an adopter with no user or
repo rules sees output byte-identical to the shipped file alone. Each layer
that fires is preceded by an HTML-comment provenance marker naming the file
it came from.

Unlike the touch config, there is **no org `extends:` hop for rules yet** —
rules have no fetch story, so a shared org constitution isn't a thing this
repo ships. The same secret-vs-shareable split still applies: your actual
rules are yours, not shippable, and belong in `~/.claude/ostrom/rules.md` (or
a private repo layer) — **outside this repo**, same as the touch config's
`secrets.yaml`. See `plugins/constitution/config/rules.example.md` for the
format (a `##` rule heading, body, then a `Source:`/`Preconditions:` HTML
comment — match `frozen-rules.md`'s own style).

## Doctor

`/doctor` runs `plugins/constitution/scripts/run-node.sh`, which resolves
Node from `PATH` or common version-manager locations and launches the
committed TypeScript bundle. It reports on seven checks: plugin installed,
marketplace clone still fast-forwardable, which rules layers actually
fired, touch-log target durability, provider reachability, local vs cloud
environment, and the supported shape of the config parser.

It exists because silent degradation is the actual failure mode here, not
a crash. The SessionStart hook injects the shipped rules and nothing else
when no user layer is present, and looks exactly like it's working. `/touch`
falls back to the `file` provider and keeps appending to a local markdown
file indefinitely, and that looks exactly like working too. Nothing errors
— touches just never reach another machine, or a documented bootstrap
one-liner 404s for months because nothing ever checked. `/doctor` is the
thing that checks: read-only, mutates nothing, and turns each of those
silent states into an `OK` / `WARN` / `FAIL` line with a concrete remedy.

## Touch-log config

`/touch` logs to a **pluggable provider**, chosen by layered YAML config
(most-specific wins): shipped defaults → `~/.claude/ostrom/config.yaml`
(user) → `./.ostrom/config.yaml` (repo) → an org config via `extends:`.
The split is **secret vs shareable**, not in-repo vs out: provider choice,
target, and bucket vocabulary are shareable; tokens live only in
`~/.claude/ostrom/secrets.yaml` (machine-local, never committed).

- **`file` (default)** — appends to `~/.claude/ostrom/touch-log.md`. Zero
  account, offline. Point it at a path inside a git repo (and set
  `auto_commit: true`) to get a versioned commons for free.
- **`notion`** — set `provider: notion` and fill the `notion:` block.

See `plugins/constitution/config/config.example.yaml` for both, and
`defaults.yaml` for what ships. `bootstrap.sh` provisions the `file`
default automatically.

## Amend (修宪)

Edit the plugin here — `frozen-rules.md`, `skills/touch/SKILL.md`, or
`config/defaults.yaml` — bump the version in `plugin.json`, push.
Environments pick it up via `/plugin marketplace update ostrom`.

## Rollback

Pin the plugin entry in `marketplace.json` to a commit SHA and push.

## License

MIT, see [LICENSE](LICENSE).
