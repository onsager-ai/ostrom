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

## What these denies are, and are not

**They are defence against inattention, not against intent.** They match command
strings, and a command string has many equivalent spellings. `gh pr merge` is
denied; the same merge through `gh api --method PUT repos/{owner}/{repo}/pulls/{n}/merge`,
through `gh api graphql` with `mergePullRequest`, through `curl`, or through the
web interface is a different string. The lists below block the mutating `gh api`
forms as well, which narrows the gap without closing it — enumerating every REST
path and spelling is a race the matcher has already lost.

Two further limits, both structural:

- **Nothing forces `--settings`.** A session launched without it carries no deny
  at all. The profile binds the session that opted into it.
- **Enforcement is client-side.** These rules are evaluated by Claude Code, so a
  path that reaches GitHub another way — an MCP tool, a hook, a helper script —
  is not matched.

The value of a client-side deny is real but specific: it converts a silent
mistake into a visible refusal. That is worth having, and it is not the same as
being unable to merge. **The only control that does not depend on the session
behaving is server-side branch protection**, and where a repository's plan does
not offer it, the separation there is advisory.

**Neither profile may deny `gh api` wholesale**, and the gatekeeper's case is
the sharper one. `gate.sh` reads review threads through `gh api graphql` —
there is no porcelain for it — so a blanket deny would make every condition
unobservable and every verdict `inconclusive`. The gatekeeper would be locked
out of the gate it exists to run, while appearing to work.

Both profiles therefore deny the **mutating** forms only, in every spelling the
matcher can name: `--method` and `-X` for PUT, POST, PATCH and DELETE, spaced,
unspaced and `=`-joined, plus graphql mutations. That narrows the surface
without severing the reads either role depends on, and it does not close the
gap — see the limits above.

**Treat the list as incomplete, because it demonstrably is.** Three separate
reviews of this one file each found a spelling the previous round had missed:
`-XPUT` unspaced, then `--method=PUT` equals-joined, then `git push -f` with
the flag last and no trailing space. Every one was a real hole, and every one
was found by someone re-reading a list its author believed was finished. Adding
the next spelling is worth doing and is not the same as closing the gap; if you
are relying on this list rather than on branch protection, you are relying on
the wrong thing.

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
      "Bash(gh pr review *)",
      "Bash(gh api *--method PUT*)",
      "Bash(gh api *--method POST*)",
      "Bash(gh api *--method PATCH*)",
      "Bash(gh api *--method DELETE*)",
      "Bash(gh api *--method=PUT*)",
      "Bash(gh api *--method=POST*)",
      "Bash(gh api *--method=PATCH*)",
      "Bash(gh api *--method=DELETE*)",
      "Bash(gh api *-X PUT*)",
      "Bash(gh api *-X POST*)",
      "Bash(gh api *-X PATCH*)",
      "Bash(gh api *-X DELETE*)",
      "Bash(gh api *-XPUT*)",
      "Bash(gh api *-XPOST*)",
      "Bash(gh api *-XPATCH*)",
      "Bash(gh api *-XDELETE*)",
      "Bash(gh api graphql*mutation*)",
      "Bash(git tag *)",
      "Bash(git push *--tags*)",
      "Bash(git push *refs/tags/*)",
      "Bash(git push *--force*)",
      "Bash(git push *-f *)",
      "Bash(git push *-f)",
      "Bash(gh release create *)",
      "Bash(gh release edit *)",
      "Bash(gh release delete *)",
      "Bash(gh release upload *)",
      "Edit(~/.claude/ostrom/mandates.yaml)",
      "Edit(~/.claude/ostrom/gate.yaml)",
      "Edit(~/.claude/ostrom/rules.md)",
      "Edit(~/.claude/ostrom/roles/**)",
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
      "Bash(gh pr edit *)",
      "Bash(gh pr close *)",
      "Bash(gh issue create *)",
      "Bash(gh issue edit *)",
      "Bash(gh issue close *)",
      "Bash(gh issue reopen *)",
      "Bash(gh pr reopen *)",
      "Bash(gh api *--method PUT*)",
      "Bash(gh api *--method POST*)",
      "Bash(gh api *--method PATCH*)",
      "Bash(gh api *--method DELETE*)",
      "Bash(gh api *--method=PUT*)",
      "Bash(gh api *--method=POST*)",
      "Bash(gh api *--method=PATCH*)",
      "Bash(gh api *--method=DELETE*)",
      "Bash(gh api *-X PUT*)",
      "Bash(gh api *-X POST*)",
      "Bash(gh api *-X PATCH*)",
      "Bash(gh api *-X DELETE*)",
      "Bash(gh api *-XPUT*)",
      "Bash(gh api *-XPOST*)",
      "Bash(gh api *-XPATCH*)",
      "Bash(gh api *-XDELETE*)",
      "Bash(gh api graphql*mutation*)",
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
