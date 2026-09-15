# Changelog

## Unreleased

- Loop wakes now fail before work when their effective repository set is empty,
  while gatekeeper wakes with a non-empty scope and no snapshot candidates end
  successfully without starting an agent session.
- Sweeps now hold an exclusive, 120-second lease across acquisition and
  generation writes, renew it every 30 seconds, and verify ownership before
  committing a generation. A killed holder expires within the short TTL, or
  is reclaimed immediately when procfs identifies it as dead. Passes wait up
  to 30 seconds for an in-flight sweep, and gatekeeper passes repair mismatched
  state and snapshot generations with one new sweep.
- Subset sweeps retain the prior generation's completion time for full-roster
  freshness, and a repository-changing pull-request repair invalidates the
  current generation.
- Worktree sweeps now remove the corresponding implementer log after removing
  an orphan or expired worktree, while retaining logs for retained worktrees.
- Every shipped prompt that invokes `ostrom gate` now requests its declared
  acquisition scope, with a test preventing any prompt from drifting.
- **Breaking:** loop declarations replace `target` with the scalar-or-list
  `repositories` field. Absent or empty means every available repository;
  manifests that still contain `target` are refused with the replacement name.
- **Breaking:** the shipped sweep preset no longer declares `loops.sweep`.
  Builder and gatekeeper passes refresh stale sweep generations before an agent
  session, and the shipped prompts no longer invoke a sweep themselves.
