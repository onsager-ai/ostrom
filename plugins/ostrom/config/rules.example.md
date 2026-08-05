# Private rules (reference example)

<!-- Copy what you want into ~/.claude/ostrom/rules.md (user layer) or
     ./.ostrom/rules.md (repo layer) and edit. This is where YOUR frozen
     rules live — the shipped frozen-rules.md deliberately stays
     content-free beyond the mechanism itself. Later layers override
     earlier ones on conflict; see the "Rules layering" section of the
     README for the order. -->

## Example: a personal escalation preference

State the rule the same way the shipped rules do: a short instruction,
in the imperative, scoped to when it applies.

<!-- Source: which correction(s) produced this rule — conversation or
     date, one line.
     Preconditions: what would invalidate it — a changed workflow,
     a tool no longer used, etc. -->

## Example: a repo-specific convention

Rules don't have to be personal; a repo layer can carry a team/project
convention that shouldn't leak into every other repo.

<!-- Source: which correction(s) produced this rule.
     Preconditions: what would invalidate it. -->
