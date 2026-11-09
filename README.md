# ostrom

A super agent for one operator. GitHub events come in, and a long-lived Router decides what is
work. Each piece of accepted work runs in its own isolated Worker session on
[T3 Code](https://github.com/pingdotgg/t3code). Follow-up discussion goes straight to the Worker
that owns the thread, and a Watchdog uses [Semon](https://github.com/onsager-ai/semon) session data
to notice stalled Workers.

Status: restarting. There is no code yet. See [`docs/direction.md`](docs/direction.md) for the
direction and [`docs/t3-feasibility.md`](docs/t3-feasibility.md) for the evidence that a daemon can
drive T3 Code. The previous Ostrom (governance CLI, signed policy, gates) is kept in Git history.

License: MIT.
