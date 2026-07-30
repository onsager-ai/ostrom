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
replace its placeholder roster. The real roster, queue, and read cursors
remain machine-local at `~/.claude/ostrom/mandates.yaml`,
`~/.claude/ostrom/queue.jsonl`, and `~/.claude/ostrom/state.json`; none
belongs in this repository.

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

The SessionStart hook never calls `gh`; it only renders the durable files
written by the scheduled sweep. Empty sections disappear, so a healthy
portfolio is exactly `N projects nominal`. If the state file is older than
`cadence_hours`, the hook adds one short stale warning. Queue rows contain
only a resolvable GitHub pointer and mandate metadata, never mirrored issue
or PR bodies. v1 implements the `file` provider only; the provider seam
remains explicit for a later addition.

Run `/desk lint` explicitly to inspect selectors that matched no open item in
the last sweep; unmatched selectors never add daily digest lines.
Baseline and mandate-change summaries render once, then are acknowledged in
the private state so they do not become permanent session noise.

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
