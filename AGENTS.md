# ostrom

A super agent: GitHub events in, a Router that dispatches, isolated Workers on T3 Code, follow-ups
routed by thread subscription, and a Watchdog. Read `docs/direction.md` first.

## Rules

1. **The loop before governance.** No policy signing, gates, manifests or verdict machinery until
   the end-to-end loop runs and a real failure calls for it. Name that failure in the PR.
2. **Do not rebuild what a neighbour owns.** T3 Code runs the sessions, threads and worktrees.
   Semon stores and queries sessions. Ostrom stores its own domain records (triggers, threads,
   decisions, tickets, assignments, subscriptions, outbox) and pointers to the rest.
3. **Deterministic first, then the model.** Filtering, dedup, fan-out and retries are code. Only
   accept/ignore judgements and reply wording go to a model.
4. **A refusal is not a success.** A command that did not do its job exits non-zero.
5. **Pin what you cross.** The T3 Code version and its contract shapes are pinned and covered by an
   agreement test, not assumed.

## Working here

- TypeScript on Node. Tests and typechecks run in CI, not on the operator's laptop.
- Use one worktree per session, and branch before you commit.
- The pre-restart code is in Git history only. Borrow pieces deliberately; nothing comes over by
  default.
