# Running Ostrom

`ostromd` is a user service. Install the two commands from a checkout with `npm link`
(they run their TypeScript directly on Node 24). Put the runtime configuration in
`~/.config/ostrom/env` (including `OSTROM_REPO`, `OSTROM_ALLOWLIST`, `OSTROM_DB`,
worker, router, and Watchdog variables). It must also set `PATH`: systemd starts user services
with a minimal one, and the daemon and its Workers need `node`, `ostromd`, `git`, `gh`, `semon`,
`codex` and `claude`. Copy your shell's value, for example with
`echo "PATH=$PATH" >> ~/.config/ostrom/env`. Then install the checked-in unit:

```sh
mkdir -p ~/.config/systemd/user
cp contrib/systemd/ostromd.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now ostromd
```

The daemon runs `ostromd run --interval 60`, restarts after failures, and writes
logs to the user journal. Inspect it with `journalctl --user -u ostromd`.

The operator uses `ostrom status`, `tickets`, `show`, `pause`, `resume`, `stop`,
`extend`, and `preview`. They use the daemon's Unix socket at
`$XDG_RUNTIME_DIR/ostrom/ostromd.sock`, or
`~/.local/state/ostrom/ostromd.sock` when `XDG_RUNTIME_DIR` is not set. The
read-only commands can inspect `OSTROM_DB` directly while the daemon is down;
mutating controls require the daemon.

Developer-only one-shot passes remain available as `ostromd dev <pass>`:
`poll`, `send`, `dispatch <ticket>`, `observe`, `commands`, `route`, `watchdog`,
`route-candidates`, and `tick`. They are retained for their existing tests and
for controlled sandbox diagnosis.

Set `OSTROM_WORKER_RULES_FILE` to a file of the machine's house rules (for example its heat
limits, and that tests run in CI rather than locally). Its text closes every Worker's brief.
