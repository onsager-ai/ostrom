# Changelog

## Unreleased

- **Breaking:** loop declarations replace `target` with the scalar-or-list
  `repositories` field. Absent or empty means every available repository;
  manifests that still contain `target` are refused with the replacement name.
- **Breaking:** the shipped sweep preset no longer declares `loops.sweep`.
  Builder and gatekeeper passes refresh stale sweep generations before an agent
  session, and the shipped prompts no longer invoke a sweep themselves.
