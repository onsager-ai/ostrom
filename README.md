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

- **`ostrom` — one plugin with two cooperating subsystems** — layered SessionStart
  constitution injection: shipped rules, then user rules, then repo rules
  (most-specific wins).
- **The rule-capitalization trigger** — the agent proposes freezing a
  rule after the same class of correction recurs; it never
  self-installs one.
- **Agent-push `/ostrom:touch` intervention capture** — with pluggable log
  providers (file/Notion) and layered YAML config.
- **Mandate portfolio steering** — an hourly sweep and SessionStart digest that reads
  open GitHub issues, PRs, and CI through `gh`; keeps a private,
  file-backed queue of pointers; and routes approve/reject/defer decisions
  through `/ostrom:desk`. Tripwires reuse constitution's escalation-dossier
  protocol and never auto-proceed.

### One-way dependency convention

The mandate subsystem may reuse the constitution subsystem's
escalation-dossier shape. The constitution subsystem must never learn about
mandates, queues, grants, or GitHub. This is a convention: nothing enforces it.
It matters because the agent-workflow constitution must remain usable and
reasoned about independently of portfolio steering, even though both now ship
inside one plugin.

## Layout

- `.claude-plugin/marketplace.json` — marketplace catalog (this repo is the marketplace)
- `plugins/ostrom/` — the unified plugin: layered rules and touch capture plus portfolio sweep, digest, private queue, and gatekeeper skills
- `plugins/ostrom/hooks/` — both SessionStart hooks: layered constitution injection and durable mandate-digest rendering
- `plugins/ostrom/config/` — separate shipped defaults for touch (`touch-defaults.yaml`) and mandates (`mandate-defaults.yaml`), plus reference examples
- `crates/ostrom-core/` — pure Rust domain types and the async, substrate-neutral store port
- `crates/ostrom-store/` — XDG paths, legacy-compatible JSONL/file persistence, and native Node runtime resolution for Codex dispatch
- `crates/ostrom-cli/` — the additive `ostrom` binary and sweep entry point
- `repo-pointer/settings.json` — snippet to merge into each target repo's `.claude/settings.json`
- `bootstrap.sh` — one command to make a fresh environment ostrom-aware (user-level enroll + config provisioning)
- `LICENSE` — MIT

### Changing shipped skills

Files matching `plugins/*/skills/*/SKILL.md` are shipped behaviour, not
documentation: builder, gatekeeper, and desk sessions execute that text from
the installed plugin cache. Changing a skill body requires changing the
`version` field in that same plugin's `.claude-plugin/plugin.json`. CI enforces
this requirement per plugin so a protocol change cannot remain hidden behind
an unchanged cache key.

### Shell implementation freeze

The implementation under `plugins/ostrom/scripts/` is frozen: new behaviour
belongs in `crates/`, while deletions from the shell are always welcome. A
small defect fix that must grow a shell file needs the `bash-bugfix` pull
request label. Issue #263 tracks removing the shell implementation entirely.

### Rust CLI (phase 2)

The Rust workspace remains additive: systemd and the plugin still invoke the
Bash scripts until an operator performs the cutover. The binary resolves config with
`ProjectDirs::from("ai", "onsager", "ostrom")` and state through the matching
XDG state directory. Setting `OSTROM_HOME` explicitly makes both roots that
directory, which is the hermetic test and parity surface.

```bash
cargo run -p ostrom-cli -- queue list --format=json
OSTROM_HOME=/path/to/ostrom-state cargo run -p ostrom-cli -- sweep
OSTROM_HOME=/path/to/ostrom-state cargo run -p ostrom-cli -- check run
OSTROM_HOME=/path/to/ostrom-state cargo run -p ostrom-cli -- plan
```

`ostrom sweep` authenticates once per distinct roster organization, performs
bounded issue, open-PR, recent-merge, and default-branch CI reads, and writes
the private queue and incremental state. Publishing is disabled unless an
explicit typed destination is supplied with `--publish-repository owner/repo`;
a scratch `OSTROM_HOME` can therefore never inherit the production hub target.
The checked-in Bash sweep remains the live fallback and is not invoked by the
Rust sweep.

`ostrom plan` refreshes stale and never-run authored mechanical criteria, runs
the same sweep, then strictly reads `goals.yaml`, mirrors durable check
receipts, derives goal facts, and writes private `plan.json` plus its
acknowledgement ledger. A goal is not semantically assessed while any cited
criterion remains stale or never run; a recorded failing verdict remains a
distinct assessable fact. Semantic assessment can use a
named harness with `--assessor[=claude|codex|copilot]`; a bare `--assessor`
selects Claude. `OSTROM_PLAN_DERIVER` accepts the same three names and remains
the arbitrary-executable escape hatch for every other value. With neither
configured, the existing `assessment_unavailable` fault and mechanical
authorization-preserving ranking are unchanged. The builder selector consumes
a plan only when its queue basis and principal `work_ranking` still match,
otherwise it visibly falls back to the existing ordering.

Every named harness may conclude only `on-track`, `at-risk`, `off-track`, or
`blocked` for the supplied goal. Claude returns its structured-output envelope,
Codex returns output checked against the same schema, and Copilot returns its
silent prompt response. This protocol difference is the full semantic choice:
all three receive only the goal input and its computed fact table, never the
backlog, and all three must cite at least one exact fact key in `because`.
Ostrom does not repair a missing or invented citation. It records
`assessment_invalid_output` and leaves the mechanical ranking in place.

Named assessment is also bounded to 20 eligible goals per plan pass. That
ceiling divides the existing default work-order ceilings ($20 and 500,000
weighted tokens) into 20 deliberately small assessments: Claude gets one turn,
a $1 API budget and a 25,000-output-token cap; Codex gets one ephemeral,
read-only response with a 25,000-token context; Copilot gets one tool-free
response with a one-AI-credit soft cap. The resulting pass bounds are therefore
$20 for Claude, 500,000 context tokens for Codex, or 20 AI credits for Copilot;
the provider-native units are kept explicit rather than pretending credits and
subscription usage are dollars. The adapters run in an empty temporary
directory with tools and network-backed context disabled. `CLAUDE_BIN`,
`CODEX_BIN`, and `COPILOT_BIN` override their executable paths.

A configured named harness that cannot be started records
`assessment_harness_unavailable`, distinct from the unconfigured
`assessment_unavailable` state. A process failure is
`assessment_harness_failed`, while malformed, uncited, mismatched, or
invented-fact output is `assessment_invalid_output`.

`ostrom migrate` moves legacy files into the XDG roots after refusing any
unexpired named lease. It rewrites in-tree private-key paths, preserves key
mode `0600`, and leaves the old directory as a compatibility pointer so the
Bash callers continue to work. Running it twice is a no-op. Stop unattended
passes before an operator performs the migration even though the command also
checks their lease files.

`ostrom-core` is not published to crates.io. Out-of-tree consumers may pin a
Git revision; registering the public crate name remains a principal decision.
The store transaction, fact-only record boundary, reusable conformance battery,
and pre-1.0 semver policy are documented in
[`docs/store-port.md`](docs/store-port.md).

## Install

### CLI (primary)

The primary CLI distribution is npm:

```sh
npm install --global @ostrom/cli
ostrom --version
```

The npm package is a thin launcher around a platform-specific optional
dependency. The compiled binary is already inside that dependency: installation
does not download or modify an executable with a lifecycle script. Prebuilt
packages cover Linux x64/arm64, macOS x64/arm64, and Windows x64.

For a source checkout, `cargo install --path crates/ostrom-cli` is the fallback.

### Claude Code plugin

```
/plugin marketplace add onsager-ai/ostrom
/plugin install ostrom@ostrom
```

Or the scripted path, from a clone of this repo:

```
./bootstrap.sh                 # user-level enroll (~/.claude/settings.json)
# then, once inside Claude Code:
/plugin install ostrom@ostrom
```

`bootstrap.sh` enrolls at the **user level**, so every repo in that
environment picks up ostrom — no per-repo pointer needed for your own
sessions. It's idempotent and backs up an existing `settings.json`. It
also drops a **zero-secret default `/ostrom:touch` config** at
`~/.claude/ostrom/config.yaml` (file provider), so logging works
immediately with no Notion account — see [Touch-log config](#touch-log-config).

Known caveat: a project's settings.json pointer registers the
marketplace but external-source plugins still need the one-time
install command per environment (claude-code issue #32606).

### Migration from the two-plugin install

The separate `constitution@ostrom` and `mandate@ostrom` installs are replaced
by the single `ostrom@ostrom` install. Commands now share the plugin namespace:
`/mandate:brief` → `/ostrom:brief`, `/mandate:desk` → `/ostrom:desk`,
`/constitution:touch` → `/ostrom:touch`, `/constitution:doctor` →
`/ostrom:doctor`, `/mandate:merge` → `/ostrom:merge`, and
`/mandate:gatekeep` → `/ostrom:gatekeep`. Machine-local config and state under
`~/.claude/ostrom/` keep their existing filenames and need no migration.

## Mandate

`mandate` resolves its small YAML schema in three layers: shipped defaults
→ `~/.claude/ostrom/mandates.yaml` → `./.ostrom/mandates.yaml`.
Copy `plugins/ostrom/config/mandates.example.yaml` to the user path and
replace its placeholder roster. The real roster, queue, read cursors, and gate
exceptions remain machine-local at `~/.claude/ostrom/mandates.yaml`,
`~/.claude/ostrom/queue.jsonl`, `~/.claude/ostrom/state.json`, and
`~/.claude/ostrom/exceptions.jsonl`; none belongs in this repository.

`search_roots` is an opt-in list of local directories. The local drift scan
discovers Git repositories beneath each root, enumerates every linked worktree,
and reports dirty, unpublished, patch-equivalent landed, and fully pushed
branches with no open or merged PR. Shipped defaults leave the list empty, so
no local paths are guessed. Run `ostrom local-drift` for detail.

Each project uses case-insensitive qualified glob selectors in `delegated`,
`excluded`, and `bounce`; `reserved` is a list of exact issue/PR numbers.
Supported selectors are `label:`, `scope:`, `type:`, `path:`, `ref:`, and
`title:`. `*` is the only wildcard (`path:**` spans directory depth), and a
`title:` selector must include `*`. Pull requests inherit the labels and refs
of their closing issues.

Each project may set `max_implementers_per_repository` to a positive integer;
omitting it defaults to 1. This per-repository limit prevents implementer
branches from colliding. It is independent of `MANDATE_MAX_IMPLEMENTERS`, the
global dispatch capacity limit for shared compute and budget.

The root `work_ranking` list records principal direction as highest-first
`owner/repo#number` pointers. It reorders only delegated work that is already
dispatchable; reserved refs, tripwires, holds, deferrals, and selector
boundaries keep precedence. The shipped empty list preserves oldest-first
selection exactly. A sweep exposes a pointer that no longer exists as a queue
and state fault rather than silently dropping it.

Classification precedence is reserved → shared/project bounce → excluded →
delegated → `default`. The default is `unclassified`, which produces one
per-repo `/ostrom:desk` triage line rather than one queue row per item; projects may
explicitly choose `default: delegated` or `default: excluded`. Pausing a
project suppresses routine work but never reserved refs, tripwires, or failing
CI. The first sweep baselines existing work, and selector changes re-baseline
scope rather than flooding the queue.

Run the sweep hourly outside Claude Code. Without an explicit
`--publish-repository`, it writes only private queue and state. Incremental runs ask the
issues REST change feed only for updates after the stored cursor and reuse each
repository's ETag, so a quiet repository receives a rate-limit-free `304`.
Open pull requests are still listed in full because check-rollup and changed-file
data can move without advancing a PR's `updatedAt`; the PR set is small, and
refreshing it prevents stale CI drift. The sweep automatically performs a full
issue reconciliation every 24 hours to remove closed items and heal a missed or
clock-skewed cursor. For example, edit the placeholder state path and install
this with `crontab -e`:

```cron
0 * * * * OSTROM_HOME=/absolute/path/to/scratch /absolute/path/to/ostrom sweep
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
and PR query reads up to 200 open items; reaching that cap is a loud fault and
the sweep refuses to write truncated state. v1 implements the `file` provider only; the
provider seam remains explicit for a later addition.

Run `/ostrom:desk lint` explicitly to inspect selectors that matched no open item in
the last sweep; unmatched selectors never add daily digest lines.
Baseline and mandate-change summaries render once, then are acknowledged in
the private state so they do not become permanent session noise.

### Sprint lease and trace

Builder wakes coordinate through a lease file at
`${CLAUDE_CONFIG_DIR:-$HOME/.claude}/ostrom/sprint.lease`. Run
`ostrom lease acquire <owner> [ttl-seconds]` before starting work,
`ostrom lease release <owner>` when finished, and `ostrom lease status`
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

Use `ostrom trace append <kind> <fact-json> <narration-json>` to write a
record. `fact` holds actions, artifacts, identifiers, external results, and
exit codes; `narration` holds reasons, beliefs, and conclusions. A builder or
gatekeeper consuming another builder's trace uses `ostrom trace read`,
which emits only `ts`, `kind`, and `fact`. Narration is for the principal and
requires the explicit `ostrom trace read-narration` verb. This structural
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
current GitHub artifacts through `/ostrom:merge`, and takes only the action the
gate permits: approve as the App and merge a pass, report a fail, or escalate an
inconclusive result to the principal. The gatekeeper does not write code,
suggest fixes, or review for quality. The builder's only response is a new
commit.

The principal starts and owns that separate gatekeeper session and answers its
escalations. The builder must not start it: whoever controls when review runs
can also decide when it does not. A different model from the builder is
recommended, not required; the separate role provides structural independence,
and a different model adds cognitive independence.

From a project where the `ostrom` plugin is installed, the principal starts a
dedicated gatekeeper session with the same recurring wake mechanism as the
sprint loop:

```sh
claude --settings ~/.claude/ostrom/roles/gatekeeper.settings.json "/loop 30m /ostrom:gatekeep"
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

### Explain policy and stalled holds

`ostrom explain owner/repository#123` evaluates the pull request against every
authored grant and deny, separates subject matching from the actor/operation
projection, names any `requires:` check and its result, and prints the aggregate
verdict with the deciding rule, scope, and source file. It discovers
`ostrom.yaml` or `ostrom.yml` from the working directory up to the repository's
`.git` boundary;
`--manifest` selects an explicit file. The deprecated `.ostrom/manifest.yml`
and user-config `manifest.yml` locations remain available during migration, but
a repository without either repository manifest is reported as ungoverned and
never falls through to the operator manifest. The separately signed operator
manifest is `<Ostrom config>/ostrom.yaml` (or `.yml`). Denies from either scope
win absolutely, a grant from either scope suffices when no deny matches, and an
unmatched request is denied. Repository `loops:` and `operations:` declarations
are reported but inert; only declarations in the operator manifest are adopted.
If both filename extensions exist for one document, loading refuses both.

The policy `defaults` map accepts `stalls_after: 7d`, which is also the default.
An individual grant or deny may override it. Sweep records the first time each
pull request resolves to the principal floor, a matching deny, or a blocked
grant requirement. Crossing the threshold adds a `STALLED HOLDS` digest
finding; it never changes `HOLD` into permission or merges the pull request.

### Selector accuracy

`/ostrom:desk lint` reports selectors that matched nothing, which is config hygiene,
not accuracy. Two different errors matter and they are not symmetric. A **miss**
is a safety failure — something crossed a boundary unreviewed. A **false alarm**
costs an interruption. Prefer recall wherever an irreversible action is in
reach and accept the precision loss there; prefer precision everywhere else,
because an interruption budget spent on noise is unavailable when it matters.

Both are measured, and neither is reduced to a single score:

- **False alarms** accrue going forward. Rejecting an item in `/ostrom:desk` appends one
  line to `~/.claude/ostrom/selector-events.jsonl` recording which selector put
  it in front of you. Nothing extra is asked at decision time.
- **Misses** are computed retroactively. `ostrom replay` is read-only: it
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
/plugin install ostrom@ostrom
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
`secrets.yaml`. See `plugins/ostrom/config/rules.example.md` for the
format (a `##` rule heading, body, then a `Source:`/`Preconditions:` HTML
comment — match `frozen-rules.md`'s own style).

## Doctor

`/ostrom:doctor` runs `ostrom doctor`, the native Rust prober. It reports on
CLI installation/version/launcher safety, plugin and marketplace integrity,
rules layering, touch durability and provider reachability, dispatch source
roots, trace/lease/work-order health, recurring delivery passes, publish
freshness, environment shape, and the supported config parser shape.

It exists because silent degradation is the actual failure mode here, not
a crash. The SessionStart hook injects the shipped rules and nothing else
when no user layer is present, and looks exactly like it's working. `/ostrom:touch`
falls back to the `file` provider and keeps appending to a local markdown
file indefinitely, and that looks exactly like working too. Nothing errors
— touches just never reach another machine, or a documented bootstrap
one-liner 404s for months because nothing ever checked. `/ostrom:doctor` is the
thing that checks: read-only against your configuration and state, and turns
each of those silent states into an `OK` / `WARN` / `FAIL` line with a
concrete remedy.

One exception, stated because the claim is otherwise not quite true: the
marketplace check runs `git fetch origin main` in the cached marketplace clone
to tell whether it is still fast-forwardable. That updates remote-tracking refs
in a cache directory. It touches no working tree, no configuration and no state
— but it is not literally nothing, and a check that overstates its own
innocence is the kind of thing this command exists to catch.

## Touch-log config

`/ostrom:touch` logs to a **pluggable provider**, chosen by layered YAML config
(most-specific wins): shipped defaults → `~/.claude/ostrom/config.yaml`
(user) → `./.ostrom/config.yaml` (repo) → an org config via `extends:`.
The split is **secret vs shareable**, not in-repo vs out: provider choice,
target, and bucket vocabulary are shareable; tokens live only in
`~/.claude/ostrom/secrets.yaml` (machine-local, never committed).

- **`file` (default)** — appends to `~/.claude/ostrom/touch-log.md`. Zero
  account, offline. Point it at a path inside a git repo (and set
  `auto_commit: true`) to get a versioned commons for free.
- **`notion`** — set `provider: notion` and fill the `notion:` block.

See `plugins/ostrom/config/config.example.yaml` for both, and
`touch-defaults.yaml` for what ships. `bootstrap.sh` provisions the `file`
default automatically.

## Amend (修宪)

Edit the plugin here — `frozen-rules.md`, `skills/touch/SKILL.md`, or
`config/touch-defaults.yaml` — bump the version in `plugin.json`, push.
Environments pick it up via `/plugin marketplace update ostrom`.

## Rollback

Pin the plugin entry in `marketplace.json` to a commit SHA and push.

## License

MIT, see [LICENSE](LICENSE).
