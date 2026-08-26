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
