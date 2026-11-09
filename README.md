<img src="docs/brand/favicon.svg" width="48" height="48" alt="Ostrom">

# ostrom

A super agent for one operator. GitHub events come in, and a Router decides what is work. Each
piece of accepted work runs in its own background Worker, a native Claude Code or Codex session in
its own worktree. Follow-up discussion goes straight to the Worker that owns the thread, and a
Watchdog uses [Semon](https://github.com/onsager-ai/semon) session data to notice stalled Workers.

Status: restarting. See [`docs/experience.md`](docs/experience.md) for the experience Ostrom is built
to, and [`docs/direction.md`](docs/direction.md) for how. The previous Ostrom (governance CLI,
signed policy, gates) is kept in Git history.

License: MIT.
