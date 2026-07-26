---
name: touch
description: Log a human intervention to the configured touch log — a
  local file by default, or Notion. Use whenever the user types /touch,
  says "记" followed by an intervention, or asks to log/record an
  intervention, correction, or judgment they just made about an agent's
  work.
argument-hint: <judgment> | <bucket> | <minutes>
---

# Touch Log

Log one intervention to the configured provider. Never assign the bucket
yourself — it is the one irreducibly human judgment.

## 1. Resolve config (layered YAML)

Read and deep-merge these layers, most-specific wins:

1. shipped defaults — `config/defaults.yaml` in this plugin (sibling of `skills/`)
2. user (shareable) — `~/.claude/ostrom/config.yaml`
3. repo — `./.ostrom/config.yaml` (if present)
4. org — whatever `extends:` points at, if any (fetch + merge under the referrer)

Merge machine-secret values from `~/.claude/ostrom/secrets.yaml` (if present)
on top — but never echo its contents.

## 2. Build the normalized record

- **judgment** — one sentence; from the argument or inferred from the session
- **date** — today, `YYYY-MM-DD`
- **system** — `claude-code-cloud` if the `CLAUDE_CODE_REMOTE` env var is set, else `claude-code-local`
- **minutes** — number
- **bucket** — one of `buckets` from the resolved config; if not supplied, ask ONLY for this (offer the configured values)
- **flags** — optional, e.g. `⚑` for an escalation-dossier override; omit if none

Infer what you can from the session; ask only for the missing bucket, nothing else.

## 3. Dispatch to the provider

**`file` (default).** Append one row to `file.path` (expand `~`). Create the
file first with this header if it doesn't exist:

```
| date | judgment | system | bucket | minutes | flags |
|------|----------|--------|--------|---------|-------|
```

Then, if `file.auto_commit` is true and the path is inside a git repo,
`git add` + `git commit` that file (one commit per touch). Otherwise just append.

**`notion`.** Create one row in `notion.data_source` via the Notion MCP
create-pages tool, mapping each normalized field through `notion.properties`
(a field with no mapping is skipped). If the Notion MCP is unavailable, fall
back to the `file` provider at `file.path` and say so.

## 4. Confirm

Confirm with the logged row / entry only. No commentary.
