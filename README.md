# ostrom

A Claude Code plugin marketplace shipping a small mechanism for
governing agent-workflow conventions, distributed to repos as a
plugin.

The spine is Elinor Ostrom's work on governing a commons: rules carry
provenance (`Source:`) and falsifiable `Preconditions:`, a human
gatekeeps what actually gets frozen, and rules have a lifecycle that
includes retirement. The engine itself is deliberately content-free —
it ships the mechanism and two generic meta-rules, not somebody's
personal constitution.

## What it ships

- **Layered SessionStart constitution injection** — shipped rules,
  then user rules, then repo rules (most-specific wins).
- **The rule-capitalization trigger** — the agent proposes freezing a
  rule after the same class of correction recurs; it never
  self-installs one.
- **Agent-push `/touch` intervention capture** — with pluggable log
  providers (file/Notion) and layered YAML config.

## Layout

- `.claude-plugin/marketplace.json` — marketplace catalog (this repo is the marketplace)
- `plugins/constitution/` — the plugin: frozen rules (injected at SessionStart), /touch skill
- `plugins/constitution/hooks/inject-constitution.sh` — SessionStart hook: emits the layered constitution (shipped → user → repo)
- `plugins/constitution/config/` — shipped defaults + reference examples for the /touch log (provider choice + layered YAML config) and for private rules (`rules.example.md`)
- `repo-pointer/settings.json` — snippet to merge into each target repo's `.claude/settings.json`
- `bootstrap.sh` — one command to make a fresh environment ostrom-aware (user-level enroll + config provisioning)
- `LICENSE` — MIT

## Install

```
/plugin marketplace add onsager-ai/ostrom
/plugin install constitution@ostrom
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
