# Merge Protocol

Evaluate exactly one pull request with the artifact-only merge gate, then merge,
comment, or escalate without reviewing or changing code. The gatekeeper pass
drives this protocol once per candidate, supplying the PR number.

Act only as the gatekeeper. Given one PR number, fetch the current artifacts,
accept the gate's verdict, and take the single action that verdict permits.

## 1. Accept exactly one pointer and resolve its repository

The input is one positive PR number and nothing else. Reject summaries,
dossiers, prior verdicts, session history, or arguments from the builder.

Before any `gh` call, resolve `owner/repo` without network access: use the
`GH_REPO` context established by the gatekeeper pass, or derive it from the current
checkout's GitHub `origin` URL for a direct Merge Protocol run. Reject the run
if this does not yield exactly one unambiguous `owner/repo`; do not guess and do
not use an ambient GitHub credential to discover it.

## 2. Authenticate through the shared App

`gatekeeper.settings.json` sets `GH_TOKEN` and `GITHUB_TOKEN` to empty, so
there is no ambient credential here to discard or fall back to by accident.
Never call `gh` directly, and never run a command that itself calls `gh`
(such as `ostrom gate`) directly either, for the rest of this protocol. A
session never needs to handle the installation token itself. Route every `gh`
call through `ostrom credential`, naming the role, repository, and complete
scope ahead of the command to run.
Confirm the locally resolved repository read-only with:

```sh
ostrom credential gatekeeper "$repository" \
  --repositories "$repository" \
  --permissions metadata:read -- \
  gh repo view "$repository" --json nameWithOwner --jq .nameWithOwner
```

`ostrom credential` mints a fresh installation token for the resolved
repository and places it only in the child environment — the token never
enters this session's shell state, is never assigned to a variable here, and
is never written to disk. Exit `111` means the credential boundary could not
start the safely authenticated command, and the given command never ran at
all; report that and stop rather than retry with an ambient credential.
Any other exit code is the given command's own, unchanged.

The required `gatekeeper` argument names the caller at the call site; it does
not narrow the shared token. Every call below explicitly supplies mandatory
repository and permission scope. Any `Ostrom-Role:` marker reaching this protocol —
on a commit or in a pull request body — was written by the role it names, so it
is a self-asserted advisory record, not evidence of who acted, and never an
input to the gate.

**A gatekeeper session that cannot mint an App token must stop, not continue as the principal.**

Continuing with an ambient token would escape the App's repository blast
radius.

Do not carry facts or conclusions from an earlier evaluation. The builder's
only reply to a verdict is a new commit; re-evaluate that new artifact from
scratch.

## 3. Run the artifact gate

`ostrom gate` calls `gh` itself, so it needs the same per-repository token as
every other call in this protocol, routed through `ostrom credential` as in step 2.
Run it once, capturing its exit code before doing anything else with it:

```sh
ostrom credential gatekeeper "$repository" \
  --repositories "$repository" \
  --permissions metadata:read,issues:read,pull_requests:read,checks:read,statuses:read,contents:read \
  -- ostrom gate "<owner/repo>#<PR number>"
gate_exit=$?
```

`contents:read` is what makes the pull request's diff readable. Without it the
diff endpoint answers `403`, every `path:` selector in `bounce_selectors` is
unobservable, and the condition reports `inconclusive` rather than the tripwire
it would have matched. The sweep's token has carried this scope all along; the
gate's did not, so the gate has been blind to path tripwires it was configured
to enforce.

`ostrom credential` exits `111` only when its credential boundary could not
start the safely authenticated command — in that case
`ostrom gate` never ran at all, and `$gate_exit` is not a verdict. Treat `111`
the same way step 2 treats an authentication failure: report it and stop,
rather than mistake it for one of `ostrom gate`'s own exit codes (`0` pass, `1`
fail, `2` inconclusive, `64` usage error). Any other value in `$gate_exit` is
`ostrom gate`'s own exit code, unchanged, and is handled as in step 4 below.
`ostrom gate` fetches the diff paths, required
checks, labels and refs, and review threads directly from GitHub. Its
review-thread query includes resolver authorship. Do not accept those inputs
from the builder. Do not re-derive, reinterpret, or override the verdict.

From the preserved verdict line, record the artifact pointer and the verdict
consumption as two distinct trace events before taking any action. Here,
`repository` is the resolved `owner/repo`, `pr_number` is the input pointer,
`gate_exit` is the preserved exit code, and `head_sha`, `verdict`, and
`already_judged` are the literal values parsed from the verdict line:

```sh
ostrom trace append artifact-produced \
  "$(jq -cn --arg repo "$repository" --argjson pr "$pr_number" \
    --arg head_sha "$head_sha" \
    '{repo: $repo, pr: $pr, head_sha: $head_sha}')" \
  '{}'
ostrom trace append gate-verdict-consumed \
  "$(jq -cn --arg repo "$repository" --argjson pr "$pr_number" \
    --arg head_sha "$head_sha" --arg verdict "$verdict" \
    --argjson exit_code "$gate_exit" \
    --arg already_judged "$already_judged" \
    '{repo: $repo, pr: $pr, head_sha: $head_sha, verdict: $verdict,
      exit_code: $exit_code, already_judged: $already_judged}')" \
  '{}'
```

These values are facts because they are identifiers and external returns, not
the gatekeeper's reasoning. If the verdict output contains a granted exception
reason, copy that external return into the verdict record's fact object as
`exception_reason`; do not move it into narration. The gatekeeper does not add
a risk assessment or reinterpretation to narration. If either append fails,
take no GitHub action; return control to the gatekeeper pass, which ends the pass
and releases its lease.

The verdict line includes
`already_judged=judged|not-judged|cannot-tell`. With a head SHA, the state is
keyed on the PR, SHA, verdict, and condition set. Without a head SHA, it is
keyed on a stable digest of the PR, verdict, and condition set. This marker
controls **delivery only** — whether an identical report was already sent. It
never changes the verdict, the action taken, or how the verdict is recorded.

A caller running this protocol on a schedule may therefore suppress a repeated
delivery for an unchanged head SHA; the gatekeeper pass does exactly that, so an
unevaluable condition escalates once instead of every wake. Suppression is the
caller's, is always about delivery, and never converts `inconclusive` into
`pass`. A new commit is a new head SHA and reports again.

## 4. Apply exactly the verdict

Every attempted write has one operation name and one exact requested scope:

- `verdict-comment` — `metadata:read,pull_requests:write`
- `merge` — `metadata:read,contents:write,pull_requests:write`

When GitHub explicitly refuses the requested authority — including `Resource
not accessible by integration`, an `addComment` or `mergePullRequest`
permission error, or an exit-`111` scope-refusal from the credential boundary —
append `write-denied` before returning. Its fact object is exactly the factual
shape below; `operation`, `requested_scope`, and the command's observed
`exit_code` must never exist only in narration:

```sh
ostrom trace append write-denied \
  "$(jq -cn --arg repo "$repository" --argjson pr "$pr_number" \
    --arg head_sha "$head_sha" --arg operation "$write_operation" \
    --arg requested_scope "$write_scope" --argjson exit_code "$write_exit" \
    '{outcome: "permission-denied", repo: $repo, pr: $pr,
      head_sha: $head_sha, operation: $operation,
      requested_scope: $requested_scope, exit_code: $exit_code}')" \
  '{}'
```

Return `permission-denied` and that fact object to the gatekeeper pass. For a
non-permission command failure, append `write-failed` with the same fields and
`outcome: "write-failed"`, then return `write-failed` and its fact object. If
either failure record cannot be appended, return a pass-ending trace error
instead; do not continue to another write after an invisible failure.

- **Pass (exit 0)** — perform these four steps in order, routing each `gh`
  call through `ostrom credential` as in step 2 so it runs with a token minted for
  this repository rather than this session's own empty credentials:
  1. Record the verdict on the pull request as a comment, writing it to a
     temporary file first so no PR-controlled text is interpolated into a
     shell command. Set `write_operation="verdict-comment"` and
     `write_scope="metadata:read,pull_requests:write"`, then run exactly:

     ```sh
     if ostrom credential gatekeeper "$repository" \
       --repositories "$repository" \
       --permissions metadata:read,pull_requests:write -- \
       gh pr comment "$pr_number" --repo "$repository" \
         --body-file "$comment_file"; then
       write_exit=0
     else
       write_exit=$?
     fi
     ```

     `gh pr comment` resolves the pull request node and invokes GraphQL
     `addComment` against that pull request subject. GitHub assigns pull
     request comments to the Pull requests permission, so `pull_requests:write`
     is the single write permission this operation needs. Do not request
     `issues:write`, `contents:write`, or any unrelated write permission.

     If the comment command fails, record and return `permission-denied` or
     `write-failed` as defined above. **The verdict comment is an audit
     prerequisite for merge: a failed comment must block the merge, and the
     merge credential must not be minted or invoked.** A merge without its
     visible verdict would be an unexplained irreversible action. This coupling
     is deliberate, not an incidental consequence of command ordering.

     **Do not approve.** `gh pr review --approve` is not part of this
     protocol and must not be added back. Every delivery role authenticates
     as the same App (#107), so the App that authored the pull request is the
     App that would review it, and GitHub refuses self-approval outright —
     there is no permission or flag that changes this. The step is not merely
     blocked; it is meaningless. An approval from the authoring identity
     asserts nothing a reader should believe.

     What the approval used to carry now lives in two better places: this
     comment, which a human browsing GitHub can see, and the `decision-taken`
     record below, which is machine-readable and carries a reversal pointer
     an approval never had.
  2. Only after the comment succeeds, set `write_operation="merge"` and
     `write_scope="metadata:read,contents:write,pull_requests:write"`, then run:

     ```sh
     if ostrom credential gatekeeper "$repository" \
       --repositories "$repository" \
       --permissions metadata:read,contents:write,pull_requests:write -- \
       gh pr merge "$pr_number" --repo "$repository"; then
       write_exit=0
     else
       write_exit=$?
     fi
     ```

     `gh pr merge` reads the pull request and invokes GraphQL
     `mergePullRequest`. Pull requests write authorizes the PR mutation and
     Contents write authorizes the resulting base-branch content change; no
     Issues permission is involved. If the merge command fails, record and
     return `permission-denied` or `write-failed` as defined above. The verdict
     comment remains a truthful record of the attempted delivery, but the
     candidate action is not `merged`.

     Do not pass `--body` here to stamp a gatekeeper role trailer. On a squash
     merge `--body` *replaces* the default commit message rather than appending
     to it, and that default is where the builder's own commits — including
     their `Ostrom-Role: builder` trailer — survive into the default branch.
     Stamping the merge would erase more attribution than it adds. The
     gatekeeper's role is recorded in the `decision-taken` record below, which
     is durable, machine-readable, and the actual audit path.
  3. Then append a `decision-taken` trace record. Merging is the
     gatekeeper's own judgment on this artifact, and it is only safe to make
     without the principal because it is cheap to undo — the reversal
     pointer is what makes that true rather than merely asserted, so it is
     recorded in the same step as the merge, not deferred to a later pass:

     ```sh
     ostrom trace append decision-taken \
       "$(jq -cn --arg owner "$lease_owner" --arg repo "$repository" \
         --arg ref "#$pr_number" --arg head_sha "$head_sha" \
         --arg reversal "revert $repository#$pr_number: open a revert pull request or \`git revert\` its merge commit" \
         '{role: "gatekeeper", owner: $owner, repo: $repo, ref: $ref,
           head_sha: $head_sha, decision: "merged pull request",
           reversal: $reversal}')" \
       "$(jq -cn --arg verdict "$verdict" \
         '{reason: ("gate verdict: " + $verdict)}')"
     ```

  4. Immediately after the `decision-taken` append, verify GitHub's
     close-keyword effect. This is an observation after the merge, not another
     gate and not permission to undo the merge or close an issue. Read the
     pull request's declared closing references, then read every referenced
     issue's current state and title, routing every read through `ostrom credential`
     with the `gatekeeper` role:

     ```sh
     declared='[]'
     still_open='[]'
     stranded='[]'
     check_errors='[]'

     if closing_result="$(
       ostrom credential gatekeeper "$repository" \
         --repositories "$repository" \
         --permissions metadata:read,issues:read,pull_requests:read -- \
         gh pr view "$pr_number" --repo "$repository" \
           --json closingIssuesReferences
     )"; then
       closing_exit=0
     else
       closing_exit=$?
     fi

     if [ "$closing_exit" -eq 0 ]; then
       if declared="$(
         jq -ce '[.closingIssuesReferences[].number]' <<<"$closing_result"
       )"; then
         closing_parse_exit=0
       else
         closing_parse_exit=$?
         declared='[]'
         check_errors="$(
           jq -cn '[{operation: "parse-closing-references"}]'
         )"
       fi
     else
       check_errors="$(
         jq -cn --argjson exit_code "$closing_exit" \
           '[{operation: "read-closing-references", exit_code: $exit_code}]'
       )"
     fi

     if [ "$(jq 'length' <<<"$check_errors")" -eq 0 ]; then
       while IFS= read -r issue_number; do
         if issue_result="$(
           ostrom credential gatekeeper "$repository" \
             --repositories "$repository" \
             --permissions metadata:read,issues:read -- \
             gh issue view "$issue_number" --repo "$repository" \
               --json number,state,title
         )"; then
           issue_exit=0
         else
           issue_exit=$?
         fi

         if [ "$issue_exit" -ne 0 ]; then
           check_errors="$(
             jq -cn --argjson errors "$check_errors" \
               --argjson number "$issue_number" \
               --argjson exit_code "$issue_exit" \
               '$errors + [{operation: "read-issue", number: $number,
                 exit_code: $exit_code}]'
           )"
           continue
         fi

         if issue="$(
           jq -ce --argjson expected_number "$issue_number" \
             'select(.number == $expected_number)
              | select(.state == "OPEN" or .state == "CLOSED")
              | select(.title | type == "string")
              | {number, state, title}' <<<"$issue_result"
         )"; then
           issue_parse_exit=0
         else
           issue_parse_exit=$?
           check_errors="$(
             jq -cn --argjson errors "$check_errors" \
               --argjson number "$issue_number" \
               '$errors + [{operation: "parse-issue", number: $number}]'
           )"
         fi
         if [ "$issue_parse_exit" -eq 0 ] && \
           [ "$(jq -r '.state' <<<"$issue")" = "OPEN" ]; then
           still_open="$(
             jq -cn --argjson open "$still_open" \
               --argjson number "$issue_number" '$open + [$number]'
           )"
           stranded="$(
             jq -cn --argjson issues "$stranded" --argjson issue "$issue" \
               '$issues + [$issue]'
           )"
         fi
       done < <(jq -r '.[]' <<<"$declared")
     fi

     if [ "$(jq 'length' <<<"$check_errors")" -ne 0 ]; then
       # The vocabulary has no success-shaped value for an observation
       # failure. Keep it in the non-success bucket and distinguish the
       # failure with the structured check_errors facts below.
       close_outcome="some-open"
       close_narration="$(
         jq -cn '{reason: "the post-merge close-keyword check could not observe every required GitHub result"}'
       )"
     elif [ "$(jq 'length' <<<"$declared")" -eq 0 ]; then
       close_outcome="none-declared"
       close_narration="$(
         jq -cn '{reason: "the pull request declared no closing references"}'
       )"
     elif [ "$(jq 'length' <<<"$still_open")" -eq 0 ]; then
       close_outcome="all-closed"
       close_narration="$(
         jq -cn '{reason: "GitHub applied every declared close keyword"}'
       )"
     else
       close_outcome="some-open"
       close_narration="$(
         jq -cn '{reason: "GitHub did not apply every declared close keyword"}'
       )"
     fi

     ostrom trace append close-keyword-checked \
       "$(jq -cn --arg owner "$lease_owner" --arg repo "$repository" \
         --arg ref "#$pr_number" --arg head_sha "$head_sha" \
         --argjson declared "$declared" --argjson still_open "$still_open" \
         --arg outcome "$close_outcome" --argjson check_errors "$check_errors" \
         '{role: "gatekeeper", owner: $owner, repo: $repo, ref: $ref,
           head_sha: $head_sha, declared: $declared, still_open: $still_open,
           outcome: $outcome, check_errors: $check_errors}')" \
       "$close_narration"
     ```

     Append that `close-keyword-checked` record exactly once, including when a
     read or parse fails. `declared` and `still_open` contain issue numbers,
     while `check_errors` contains each failed operation, the referenced issue
     number when known, and the observed exit code when a command ran. Thus no
     issue number, state, outcome, or check failure exists only in narration.
     `some-open` is deliberately the conservative non-success outcome when
     `check_errors` is non-empty; it does not assert that an unobservable issue
     was confirmed open, because `still_open` contains only confirmed `OPEN`
     issues.

     After the append, include the result in the user-facing report for this
     pull request:

     - For every object in `stranded`, say that
       `<owner>/<repo>#<number> — <title>` remained open after the merge. Name
       every stranded issue; do not report this as an ordinary successful
       merge.
     - If `check_errors` is non-empty, report that the post-merge check failed,
       name each failed operation and its exit code when present, and do not
       claim that the declared issues all closed. Exit `111` specifically means
       the App authentication failed and the `gh` read never ran.
     - Only an error-free `all-closed` or `none-declared` result may use the
       ordinary successful-merge report.

     The merge has already happened. Do not revert it and do not close a
     stranded issue. A failure here is recorded and reported before stopping;
     it is never retried with an ambient credential.

  **Every one of these calls must go through `ostrom credential`, never the
  principal's account.** Removing the approve step removed the place where a
  fallback to the principal's identity used to be caught early, so nothing
  now fails before the merge itself. Route every call through the wrapper. An
  exit `111` before the merge is a stop; in post-merge step 4 it is first
  recorded and reported as the already-merged check failure described there.

  An `excused` condition is part of a `pass`, so the merge proceeds normally.
  The exception reason already appears in the verdict output and the
  `gate-verdict-consumed` trace fact.
- **Fail (exit 1)** — if `already_judged=not-judged`, leave the complete gate output
  as one PR comment using the same `verdict-comment` operation and exact
  `metadata:read,pull_requests:write` scope declared above; use a temporary file
  so PR-controlled text is never interpolated into a shell command. Apply the
  same `permission-denied` / `write-failed` recording rules if the comment is
  refused. If `already_judged=judged`, do not post again. If it is
  `cannot-tell`, make no GitHub write and report the gate's named judgment-history
  error to the principal. Stop in all three cases.
- **Inconclusive (exit 2)** — address the principal and emit exactly this
  dossier shape, populated from the gate's unobservable condition details:

  ```text
  Question: Should the principal wait for an observable gate result or decide this pull request outside the gate?
  Options ruled out: The gatekeeper inferring missing facts; treating inconclusive as pass or fail; asking the builder to argue the existing artifact.
  Recommended action: The principal chooses whether to wait and re-run or use the one-PR exception path by running `ostrom excuse grant <owner/repo>#<PR number> <condition> <reason...>`, then re-run the gate.
  Blast radius: This pull request only; no standing permission and no change to gate conditions.
  ```

  Do not comment on the PR and do not merge.

Any other exit code is a gate execution failure. Treat it as inconclusive,
include the observed exit code in the Question field, address the same dossier
to the principal, and stop.

**Never merge on `fail`, `inconclusive`, or any other exit code**, and never
post a verdict comment that reads as a pass. On a non-pass verdict the
existing behaviour is unchanged: comment the gate output or escalate, and
stop. The merge is now the first irreversible action in this protocol rather
than the second, so there is no earlier step left to catch a mistake.

## 5. Stay narrow

Never fix code, edit files, dismiss review threads, rebase, resolve conflicts,
suggest fixes, or review for quality. The gatekeeper is a judge, not a
second author. Do not debate the builder and do not accept an argument for an
unchanged head SHA.

### Resolving a review thread

Resolving a thread is the one exception, and it is not a widening: every other
item in the list above is an **authoring** action, while judging that a thread
has been addressed is what a judge is for. It sat in the wrong list.

The condition cannot be satisfied any other way. `ostrom gate` counts a thread
resolved by the PR author as unresolved, and the builder and the principal
share one GitHub account, so neither can clear one. Automated reviewers comment
on most pull requests and do not resolve when their point is addressed, so a
new commit arrives with the thread still open. Without this, `review_threads`
is not strict — it is unclearable, and every merge needs a principal exception.

Resolving still means calling `gh`, so it is still bound by step 2: route the
`gh api graphql` call that resolves the thread through `ostrom credential` the
same way, naming `gatekeeper`, this repository, and the explicit
`metadata:read,pull_requests:write` permission scope ahead of that mutation.

Resolve a thread only when **all** of these hold:

1. You have read the thread and the diff, and confirmed in the artifact that
   the change addressing it is present at the current head SHA.
2. You state the commit SHA that addresses it in the resolving comment. A
   resolution with no named commit is indistinguishable from clearing a thread
   to unblock yourself, which is the thing this permission must not become.
3. The point is genuinely addressed. A thread's reply settles into one of
   three distinct shapes; treat them as distinct rather than reducing the
   third to the second:

   - **Fixed at this head.** The addressing change is present in this pull
     request's diff. Resolve it, naming that commit per (2).
   - **Fixed, but by a different pull request.** The reviewer's point was
     correct, the author accepted it, and a separate, already-merged pull
     request applied the fix. Check this against `main`, not against this
     diff — the question "is the fix on `main`?" has a yes-or-no answer, so
     this is not a judgment call any more than (1) is, only against a
     different ref. Resolve it, naming the commit on `main` that fixed it.
   - **Argued, not fixed.** The author replied explaining why no change was
     warranted here, and nothing external settles it. That is the author
     arguing an unchanged artifact — leave it open and let the verdict stand.

   The first two are objective and resolvable; only the third is a standing
   disagreement with no exit through this protocol, and it is the only one of
   the three that should ever be reported as "the author argues."

Immediately after resolving, append a `decision-taken` trace record naming the
same commit and pointing back at the thread. This is what lets a principal who
finds a thread resolved wrongly unresolve it directly, rather than
reconstructing the judgment from the PR history:

```sh
ostrom trace append decision-taken \
  "$(jq -cn --arg owner "$lease_owner" --arg repo "$repository" \
    --arg ref "#$pr_number" --arg thread_id "$thread_id" \
    --arg commit "$addressing_commit" \
    --arg reversal "unresolve thread $thread_id on $repository#$pr_number" \
    '{role: "gatekeeper", owner: $owner, repo: $repo, ref: $ref,
      decision: "resolved review thread", thread_id: $thread_id,
      reversal: $reversal}')" \
  "$(jq -cn --arg commit "$addressing_commit" \
    '{reason: ("addressed at " + $commit)}')"
```

A thread you cannot evaluate stays open, and the condition stays `fail`. Being
unable to judge is a legitimate outcome; the escalation dossier exists for it.

**Dismissing a review is still forbidden.** Resolving says "this was
addressed"; dismissing says "this does not matter". Only the principal says the
second, and the difference is exactly the authority the gatekeeper does not
have.

You still cannot write code, so you cannot manufacture the fix you are
verifying. That is what makes this safe rather than a loosening — and it is why
the same permission would be indefensible for the builder.

Confirm only the artifact pointer, head SHA, gate verdict, and the action
taken. No retrospective commentary.
