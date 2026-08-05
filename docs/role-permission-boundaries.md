# Claude Code permission boundaries for delivery roles

These are installation instructions for the principal. They are proposals,
not repository-enforced policy, and nothing in this repository installs them.
The principal creates the two role profiles below and launches each delivery
role with its matching `--settings` file. The principal does not use either
profile.

Claude Code deny rules are evaluated before ask and allow rules. The current
syntax uses command globs such as `Bash(git push *)` and gitignore-style edit
paths such as `Edit(~/.claude/ostrom/gate.yaml)`. The profiles also disable
permission-bypass mode. These controls are defence in depth. GitHub branch
protection is the enforceable merge boundary only after the builder and
gatekeeper authenticate as distinct GitHub identities, because only then can
the server distinguish their authority. A fresh installation starts with both
roles sharing the principal's identity; in that state, the branch-protection
claim does not hold.

## GitHub App identity prerequisite

Before enabling branch protection or starting a gatekeeper session, the
principal completes the GitHub App setup decided in
[#29](https://github.com/onsager-ai/ostrom/issues/29):

1. Create a GitHub App with Pull requests read/write, Contents read/write,
   Checks read, and Metadata read permissions, then install it only on the
   repositories the gatekeeper covers.
2. Store its machine-local credentials outside every repository at
   `~/.claude/ostrom/secrets.yaml` (or the equivalent path below
   `CLAUDE_CONFIG_DIR`) using this shape:

   ```yaml
   gatekeeper:
     app_id: <APP_ID>
     private_key_path: <ABSOLUTE_PATH_TO_PRIVATE_KEY>
   ```

   The installation is resolved from each `owner/repo` at mint time. A legacy
   `installation_id` entry is obsolete, is ignored for backward compatibility,
   and may be deleted.

3. Launch the gatekeeper with the profile below. The profile clears inherited
   GitHub tokens, and `/gatekeep` or `/merge` must successfully mint a fresh
   App installation token for each repository before making any `gh` call
   against it.

Until these steps are complete, the builder and gatekeeper remain the same
GitHub actor: author-resolved threads cannot be distinguished from legitimate
gatekeeper resolutions, branch protection cannot enforce the role split, and
`merged_by` has no independent audit value.

## Builder profile

The principal puts this JSON at
`~/.claude/ostrom/roles/builder.settings.json` and launches the builder with:

```sh
claude --settings ~/.claude/ostrom/roles/builder.settings.json
```

```json
{
  "$schema": "https://json.schemastore.org/claude-code-settings.json",
  "permissions": {
    "disableBypassPermissionsMode": "disable",
    "deny": [
      "Bash(gh pr merge *)",
      "Bash(git tag *)",
      "Bash(git push *--tags*)",
      "Bash(git push *refs/tags/*)",
      "Bash(gh release create *)",
      "Bash(gh release edit *)",
      "Bash(gh release delete *)",
      "Bash(gh release upload *)",
      "Edit(~/.claude/ostrom/mandates.yaml)",
      "Edit(~/.claude/ostrom/gate.yaml)",
      "Edit(/.ostrom/mandates.yaml)",
      "Edit(/.ostrom/gate.yaml)"
    ]
  },
  "sandbox": {
    "filesystem": {
      "denyWrite": [
        "~/.claude/ostrom/mandates.yaml",
        "~/.claude/ostrom/gate.yaml",
        ".ostrom/mandates.yaml",
        ".ostrom/gate.yaml"
      ]
    }
  }
}
```

The builder remains able to commit, push, open PRs, rebase, and resolve code
conflicts. It cannot merge PRs, edit the mandate roster or gate conditions, or
create tags and releases through the named ordinary commands.

## Gatekeeper profile

The principal puts this JSON at
`~/.claude/ostrom/roles/gatekeeper.settings.json` and launches the gatekeeper
with:

```sh
claude --settings ~/.claude/ostrom/roles/gatekeeper.settings.json
```

```json
{
  "$schema": "https://json.schemastore.org/claude-code-settings.json",
  "env": {
    "GH_TOKEN": "",
    "GITHUB_TOKEN": ""
  },
  "permissions": {
    "disableBypassPermissionsMode": "disable",
    "deny": [
      "Edit",
      "Write",
      "Bash(git add *)",
      "Bash(git am *)",
      "Bash(git cherry-pick *)",
      "Bash(git commit *)",
      "Bash(git merge *)",
      "Bash(git rebase *)",
      "Bash(git push *)",
      "Bash(gh pr create *)",
      "Bash(git tag *)",
      "Bash(gh release create *)",
      "Bash(gh release edit *)",
      "Bash(gh release delete *)",
      "Bash(gh release upload *)"
    ]
  },
  "sandbox": {
    "filesystem": {
      "denyWrite": [
        "~/.claude/ostrom/mandates.yaml",
        "~/.claude/ostrom/gate.yaml",
        ".ostrom/mandates.yaml",
        ".ostrom/gate.yaml"
      ]
    }
  }
}
```

The gatekeeper can read artifacts and run `gh pr merge`. It cannot write code,
stage or commit changes, push, open PRs, rebase or resolve code conflicts, edit
the mandate roster or gate conditions, or create tags and releases through the
named ordinary commands.

## Principal

The principal installs neither delivery-role profile. The principal's normal
user settings remain at `~/.claude/settings.json`; no deny rules are proposed
for the principal here. The principal alone may edit
`~/.claude/ostrom/mandates.yaml`, `~/.claude/ostrom/gate.yaml`, or a repository's
`.ostrom/mandates.yaml` and `.ostrom/gate.yaml`, resolve or dismiss review
threads, merge outside the gate for one explicitly named PR, and create tags or
releases.

## Review-thread boundary cannot be expressed precisely

There is deliberately no `Bash(gh api graphql *)` deny in either profile.
GitHub has no `gh` porcelain command for resolving a review thread. Reading a
thread and calling the `resolveReviewThread` mutation both use `gh api
graphql`. A payload-text deny is not a boundary: `--input`, `-F query=@file`,
and harmless whitespace changes all bypass it. A binary/subcommand deny would
also block the gatekeeper's required read.

Do not replace that omission with a coarse deny that blocks unrelated GitHub
work. Once the builder and gatekeeper use distinct GitHub identities, branch
protection is the enforceable server-side boundary; in a fresh
shared-identity installation it is not. The `gh pr merge` deny for the builder
remains client-side defence in depth.
For the thread-specific conflict of interest, `gate.sh` independently treats
every thread resolved by the PR author as unresolved, so self-resolution
cannot satisfy the condition. Stronger prevention requires a GitHub-side
control or separate OS identity administered by the principal, not another
fragile Claude Code command pattern.
