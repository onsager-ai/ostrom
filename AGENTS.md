# ostrom

A super agent: GitHub events in, a Router that dispatches, isolated background Workers as native
Claude and Codex sessions, follow-ups routed by thread subscription, and a Watchdog. Read
`docs/experience.md`, then `docs/direction.md`.

## Rules

1. **The loop before governance.** No policy signing, gates, manifests or verdict machinery until
   the end-to-end loop runs and a real failure calls for it. Name that failure in the PR.
2. **Do not rebuild what a neighbour owns.** The Claude Agent SDK and the Codex SDK run the agent
   loop. Semon reads sessions. T3 Code is the operator's foreground, and Ostrom never creates T3
   threads. Ostrom stores its own domain records (triggers, threads, decisions, tickets,
   assignments, subscriptions, outbox) and pointers to the rest.
3. **Deterministic first, then the model.** Filtering, dedup, fan-out and retries are code. Only
   accept/ignore judgements and reply wording go to a model.
4. **A refusal is not a success.** A command that did not do its job exits non-zero.
5. **Pin what you cross.** The SDK versions and the shapes Ostrom reads from them are pinned and
   covered by agreement tests, not assumed.

## Working here

- TypeScript on Node. Tests and typechecks run in CI, not on the operator's laptop.
- Use one worktree per session, and branch before you commit.
- The pre-restart code is in Git history only. Borrow pieces deliberately; nothing comes over by
  default.

<!-- agent-config:begin -->
## Shared agent conventions (generated)

- **git-hooks:** Enable generated repository hooks once per clone with `git config core.hooksPath .githooks`; in linked worktrees this relative path resolves to each checkout's own `.githooks`. Never use `--no-verify`. A passing hook, or an unconfigured hook, is not formatter evidence: run and report the repository gate's real result, for example `cargo fmt --all -- --check; echo exit=$?`.
<!-- agent-config:end -->
