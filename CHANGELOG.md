# Changelog

## Unreleased

- Loop wakes now fail before work when their effective repository set is empty,
  while gatekeeper wakes with a non-empty scope and no snapshot candidates end
  successfully without starting an agent session.
- Sweeps now hold an exclusive lease across acquisition and generation writes;
  passes wait up to 30 seconds for an in-flight sweep, and gatekeeper passes
  repair mismatched state and snapshot generations with one new sweep.
- Subset sweeps retain the prior generation's completion time for full-roster
  freshness, and a repository-changing pull-request repair invalidates the
  current generation.
- The gatekeeper prompt now requests every read permission used by `ostrom gate`,
  with a test tying the example to the gate's declared acquisition scope.
- **Breaking:** loop declarations replace `target` with the scalar-or-list
  `repositories` field. Absent or empty means every available repository;
  manifests that still contain `target` are refused with the replacement name.
- **Breaking:** the shipped sweep preset no longer declares `loops.sweep`.
  Builder and gatekeeper passes refresh stale sweep generations before an agent
  session, and the shipped prompts no longer invoke a sweep themselves.
