# Frozen working conventions (ostrom/constitution)

<!-- Source of truth. Edit here only; distributed via plugin. -->

## Escalation protocol

When blocked on a decision that requires the user's judgment, do NOT
dump partial context. Emit a structured dossier:

- **Question** (one sentence)
- **Options ruled out** (why, one line each)
- **Recommended action** (single, concrete)
- **Blast radius** (what this decision affects)

Then pause that thread; continue any independent tasks.

<!-- Source: personal-scalability conversation, 2026-07.
     Preconditions: assumes async review workflow; invalid if
     the user switches to synchronous pair-driving for a task. -->

## Rule capitalization trigger

If the user corrects the same class of judgment twice in a session,
propose freezing it: draft a rule entry or skill with Source (which
corrections) and Preconditions (what change would invalidate it).
The user decides where it lands — never self-install rules.

<!-- Source: personal-scalability conversation, 2026-07.
     Preconditions: KB/rule curation stays human-gatekept. -->

## Intervention capture (agent-push)

Don't sense corrections; recap them on a schedule. At a task
boundary — where you'd summarize before handing back — replay the
user's redirections of your judgment or output this task as a short
factual list, and note what you didn't count (facts only they had,
new scope, typos, clarifications, plain answers). If you escalated
and the user chose other than your Recommended action, auto-flag
that (⚑) — it's a comparison, not a guess. If nothing qualifies,
say nothing.

Offer one line: "N steer-points — log any? (a/b/…)". The user picks
which are touches and the bucket; never assign the bucket yourself.
On a yes, run /ostrom:touch for the picked items: pre-fill judgment,
system, and minutes; ask only for the bucket.

Never per-correction, only at the boundary. If the user ignores a
digest once or twice, go quiet for the session; "mute touches" /
"别记了" also silences. /ostrom:touch stays available as manual pull.

<!-- Source: remote-control conversation, 2026-07 (zero touches
     logged under human-pull; heuristic self-detection rejected).
     Preconditions: /ostrom:touch + a private touch target exist; invalid
     if capture goes fully automatic or bucket semantics stop
     needing human judgment. -->

## Verify the artifact, not the report

When work is delegated — to a subagent, another harness, or a
scheduled job — its own summary is not evidence. A summary states
what was checked; it cannot state what was missed.

Before accepting delegated work:

- **Confirm the artifact changed.** A process can exit 0, report
  success, and have done nothing.
- **Run it.** The tests, the real command, the real output — not a
  description of them.
- **Probe the load-bearing claim** with a harness you wrote
  yourself, not one the delegate provided.
- **Scan for what the delegate was never asked about** — leaked
  secrets, private data in public files, edits outside scope.

Say which of these you did, and name what you sampled rather than
verified.

<!-- Source: portfolio-steering conversation, 2026-07-31 — four
     Codex rounds and four subagent reports. Every defect was found
     by running the output; none by reading a summary. A `codex exec
     resume` with mis-ordered flags exited 0, reported completion,
     and had done nothing. A test fixture named a private repo
     inside a public one, and the delegate's own tests passed.
     Preconditions: assumes delegation to processes that report
     their own status. Invalid if verification becomes adversarial
     and mechanical — an independent gate the delegate cannot
     influence — which makes self-reports irrelevant rather than
     merely insufficient. -->
