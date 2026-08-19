# Harness and App boundaries for delivery roles

These are installation instructions for the principal. They are proposals,
not repository-enforced policy, and nothing in this repository installs them.
The principal creates the two role profiles below and launches each delivery
role with its matching `--settings` file. The principal does not use either
profile.

The security model is deliberately split:

> **The harness is the enforcement boundary. The App is the blast radius.**

The shared GitHub App bounds which repositories and GitHub capabilities a
compromised delivery loop can reach at all. It does not distinguish builder,
gatekeeper, sweep, or a future role: GitHub grants pull-request writes as one
permission, so an App that can open a pull request can also merge one. The
role settings and protocols decide which role may exercise each capability
inside that blast radius.

The role argument to `ostrom credential` remains required even when every role
resolves to the shared credential. It names the caller at the call site,
making the route legible; it is not an access control. Every call also
requires a non-empty repository set and permission map. The binary does not
infer scope from the role or default to the App's ceiling.

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

The value of a client-side deny is real but specific: inside the supported
harness it converts a forbidden role action into a visible refusal. It does
not make the shared App incapable of the action. A session launched outside
the profile, or a command path the matcher does not cover, is outside this
role boundary but remains inside the App's repository and permission blast
radius.

**Neither profile may deny `gh api` wholesale**, and the gatekeeper's case is
the sharper one. `ostrom gate` reads review threads through `gh api graphql` —
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
the next spelling is worth doing and is not the same as closing the gap.

## Shared GitHub App prerequisite

The existing writer App is the one credential for every delivery role. Its
machine-local credentials live outside every repository at
`~/.claude/ostrom/secrets.yaml` (or below `CLAUDE_CONFIG_DIR`) in exactly this
shape:

```yaml
shared:
  app_id: <APP_ID>
  private_key_path: <ABSOLUTE_PATH_TO_PRIVATE_KEY>
```

No real App ID or key path belongs in a repository. A legacy
`installation_id` field is accepted and ignored, but should be removed. During
cutover, a role-named block such as `builder:` or `gatekeeper:` takes
precedence over `shared:` so rollback remains a one-block config change. The
steady-state file has only the `shared:` block.

`ostrom credential` receives the role, a lookup `owner/repo`, and mandatory
`--repositories` and `--permissions` scope. It resolves the credential by role
first and `shared:` second, mints a JWT in memory, looks up the installation
for that repository, verifies that the installation holds the requested
permissions, and sends both scope fields in the token exchange. A token is
scoped to one installation and only the named repositories; a token minted
for a repository in one organisation must not be reused for another
organisation's repositories. The command contains the token in the child's
environment and never falls back to ambient `GH_TOKEN` or `GITHUB_TOKEN` when
minting fails. The granted repository and permission scope is recorded as an
`installation-token-minted` trace fact; credential and installation
identifiers are not recorded.

The shared App's installation must therefore cover **every repository in the mandate roster**, in every organisation the roster spans — not only the ones with an open pull request at any given moment. The native sweep reads issues, pull requests, default-branch CI, and commit history across the whole roster on every run, and mints one token per organisation for that reason ([#106](https://github.com/onsager-ai/ostrom/issues/106)). A repository the App is not installed on is an authentication fault there, not an empty result, and the sweep must report it as one — a silently empty queue reads as a healthy, quiet portfolio.

There is no second read-only App. Every role runs on the same machine and can
reach the same plugin cache and secrets file, so a second key would not form a
meaningful isolation boundary in this deployment. It would add another
credential to hold and rotate without materially reducing the shared-machine
blast radius.

Retire a former role App only after a real end-to-end pass has used the shared
credential. Keep the old key until then: while it remains, rollback is a
one-block configuration change.

## Advisory role attribution

Because GitHub renders every delivery action as the same App actor, builder
commits and pull request bodies carry `Ostrom-Role: builder`. The marker is
written by the same agent it names. It is a record for human legibility, not a
control or evidence of who acted; no gate, audit, or authorization decision may treat it as proof.

The gatekeeper deliberately does not stamp the merge commit. `gh pr merge
--body` replaces the squash commit message rather than appending to it, and
that default message is what carries the builder's own commits — trailer
included — onto the default branch. Stamping the merge would delete more
attribution than it adds. The gatekeeper's role is recorded in its
`decision-taken` trace record, which is durable and machine-readable, and is
where an audit should look.

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
      "Edit(~/.claude/ostrom/exceptions.jsonl)",
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
        "~/.claude/ostrom/exceptions.jsonl",
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
      "Bash(git tag *)",
      "Bash(gh release create *)",
      "Bash(gh release edit *)",
      "Bash(gh release delete *)",
      "Bash(gh release upload *)",
      "Edit(~/.claude/ostrom/exceptions.jsonl)"
    ]
  },
  "sandbox": {
    "filesystem": {
      "denyWrite": [
        "~/.claude/ostrom/mandates.yaml",
        "~/.claude/ostrom/gate.yaml",
        "~/.claude/ostrom/exceptions.jsonl",
        ".ostrom/mandates.yaml",
        ".ostrom/gate.yaml"
      ]
    }
  }
}
```

The gatekeeper can read artifacts, request an approval, and run `gh pr merge`.
It cannot write code, stage or commit changes, push, open PRs, rebase or
resolve code conflicts, edit the mandate roster or gate conditions, or create
tags and releases through the named ordinary commands. GitHub actor-based
checks still see the same App as the builder; the profile, not the actor name,
is the role boundary.

## Principal

The principal installs neither delivery-role profile. The principal's normal
user settings remain at `~/.claude/settings.json`; no deny rules are proposed
for the principal here. The principal alone may edit
`~/.claude/ostrom/mandates.yaml`, `~/.claude/ostrom/gate.yaml`, or a repository's
`.ostrom/mandates.yaml` and `.ostrom/gate.yaml`, dismiss reviews, grant a gate
exception for one explicitly named PR artifact, and create tags or releases.

## One-PR exception path

An exception is a machine-local event, not gate policy. The principal grants
one with:

```sh
ostrom excuse grant <owner/repo>#<number> <condition> <reason...>
```

The verb accepts only `required_checks`, `review_threads`,
`bounce_selectors`, `reserved_refs`, or `merge_protocol`, requires a non-empty reason, resolves
the pull request's current head SHA itself, and appends the grant to the
resolved Ostrom state directory (or `OSTROM_HOME` when explicitly set). Run
`ostrom excuse list [<owner/repo>#<number>]` to see append order and whether each
record is current or superseded by the pull request's present head SHA.

The next gate run evaluates every condition normally, then reports a matching
failed or inconclusive condition as `excused`, with its reason. An excused
condition satisfies aggregation but is never represented as a per-condition
`pass`. The grant is scoped to `(repo, PR, head SHA, condition)`: it remains
effective across legitimate re-runs of the same artifact and silently stops
matching after a new commit. Records are never consumed or deleted.

Both delivery profiles deny writes to this log. The builder cannot grant its
own exception, and the gatekeeper cannot append an exception for the condition
blocking it. The latter matters equally: that ability would let the gatekeeper
manufacture the authority it is meant only to act on. The principal grants;
the gatekeeper acts on the resulting pass and merges.

## Shared-actor limitations that remain

There is deliberately no `Bash(gh api graphql *)` deny in either profile.
GitHub has no `gh` porcelain command for resolving a review thread. Reading a
thread and calling the `resolveReviewThread` mutation both use `gh api
graphql`. A payload-text deny is not a boundary: `--input`, `-F query=@file`,
and harmless whitespace changes all bypass it. A binary/subcommand deny would
also block the gatekeeper's required read. Do not replace that omission with a
coarse deny that blocks unrelated GitHub work. The `gh pr merge` deny for the
builder remains a harness control; the shared App itself can merge.

Two existing actor-based assumptions do not survive the shared App cutover:

- GitHub does not accept an approval from the pull request's own actor. A pull
  request opened by the shared App and an approval attempted by the
  gatekeeper are the same actor to GitHub, even though the harness roles differ.
- `ostrom gate` counts a thread resolved by the pull request author as unresolved.
  A gatekeeper resolution now has that same actor too, so the gate cannot
  distinguish it from builder self-resolution.

`Ostrom-Role` markers cannot repair either assumption because they are
self-asserted and advisory. The approve-first and author-resolver paths must be
changed or explicitly excepted before an end-to-end shared-App pass can prove
the cutover. This is an operational precondition, not evidence for retaining
per-role Apps.

**The gatekeeper profile therefore carries no graphql-mutation deny, and the
builder's stays.** Resolving a review thread is the `resolveReviewThread`
mutation — there is no porcelain for it — so that deny would forbid the
gatekeeper the write its protocol assigns it without fixing the shared-actor
conflict. Before adding a deny to the gatekeeper, check what `ostrom gate` and
`/ostrom:merge` actually call and settle the actor-independent thread rule.

That deny was never load-bearing anyway: it matches only the literal word
`mutation` in the command string, so `--input`, `-F query=@file` and a
whitespace change all pass it. It is kept for the builder as the same kind of
visible-refusal defence as the rest of the list, and for no stronger reason.
