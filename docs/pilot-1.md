# First real-repository pilot

Status: passed, 2026-11-17. All six issues became merged PRs, authored and merged by the GitHub
App, on two real repositories: `onsager-ai/duhem` and `onsager-ai/tolmap`. Each repository's
Manager filed or chose the issues, reviewed every PR and approved it. Neither Manager merged or
pushed anything itself. The run also found eight defects in Ostrom, listed below. All times are
UTC.

## Setup

- Host hp-code-2. One `ostromd` per repository, run from main at `3f8d003`, later `4907ba8`. Each had
  its own store, its own fresh clone under `~/projects/onsager-ai/ostrom-pilot/`, and its own
  `XDG_RUNTIME_DIR`, so the two sockets didn't collide.
- Merge policy `approvals`, one approval, approver `tikazyq`. Allowlist `tikazyq`. Router and
  Workers: Codex `gpt-6-luna`.
- Starting budget: 60 minutes and 20 turns per ticket, with the default token limit of 1,000,000.
  Some tickets were extended during the run (see below).
- Each repository's conventions went into the Worker brief, including commit titles, CHANGELOG rules,
  hooks, files not to touch and work in flight.
- tolmap requires the check `build, lint, offline parity gate`. duhem has branch protection with no
  required checks.

## Results

| Repository | Issue | PR | Opened | Merged | Review rounds | Turns | Fresh tokens |
|---|---|---|---|---|---|---|---|
| tolmap | #327 `--version` | [#330](https://github.com/onsager-ai/tolmap/pull/330) | 02:12 | 02:38 | 0 | 2 | 0.14M |
| tolmap | #328 help text | [#331](https://github.com/onsager-ai/tolmap/pull/331) | 02:44 | 03:22 | 1 | 3 | 0.29M |
| tolmap | #329 `repos --format text` | [#332](https://github.com/onsager-ai/tolmap/pull/332) | 03:29 | 04:11 | 1 | 4 | 0.47M |
| duhem | #392 editor setup docs | [#689](https://github.com/onsager-ai/duhem/pull/689) | 02:08 | 03:52 | 1 | 11 | 0.72M |
| duhem | #634 `--db` precedence | [#690](https://github.com/onsager-ai/duhem/pull/690) | 02:09 | 04:13 | 1 | 13 | 1.04M |
| duhem | #644 `--filter` verdict docs | [#691](https://github.com/onsager-ai/duhem/pull/691) | 02:15 | 04:33 | 1 | 16 | 2.11M |

Every merge was made by `ostrom-agent[bot]` (`origin=ostrom` in `pr_merge`). The tolmap merges are
the first live run of the protection gates. Until now, unit tests were their only evidence.

What worked, in the Managers' words: the first drafts were close, and each review round went to the
same Worker session, which fixed exactly what was asked within minutes and touched nothing else. The
Router never acted on any of the Managers' other comments. The tolmap tickets ran one at a time,
because all three edited `src/main.rs`. They needed no human help beyond review.

The duhem tickets ran in parallel. Each one took 5 to 8 times the turns of a tolmap ticket, and that
extra cost came from the defects below, not from the tasks.

## Defects found

| Issue | What happened |
|---|---|
| [#725](https://github.com/onsager-ai/ostrom/issues/725) | Every approval woke the Worker, which spent a turn saying nothing needed changing and restarted the quiet window |
| [#729](https://github.com/onsager-ai/ostrom/issues/729) | The Worker's report went to GitHub verbatim: links to host paths, "I did not comment on GitHub" posted as a comment, and "CI is green" on a PR that conflicted with main |
| [#730](https://github.com/onsager-ai/ostrom/issues/730) | Codex's sandbox keeps `.agents/` read-only, so a Worker couldn't edit a repository-owned skill source. Kept as is; the duhem Manager made that edit |
| [#731](https://github.com/onsager-ai/ostrom/issues/731) | A time budget that ran out during review became a public "stuck" state. `extend` didn't reopen the ticket, a reply on the issue after `resume` didn't clear the stall, and clearing it through a review failed one tick per ticket on an `open → open` transition |
| [#732](https://github.com/onsager-ai/ostrom/issues/732) | Approvals on an older head don't count, which is correct, but nothing said so. GitHub and Ostrom's own comments showed the PRs as approved |
| [#733](https://github.com/onsager-ai/ostrom/issues/733) | Parallel CHANGELOG edits cascade. Each merge left the remaining PRs conflicted, and each conflict cost a merge-main round, a CI run and a fresh approval. #691 needed three |

Operational lessons, not Ostrom code:

- The launcher's `thermal gate --max-seconds 5400` stopped both daemons after 90 minutes. A restart
  resumed every session by its ID (`3 resumed`, then `1 resumed, 1 interrupted`), and the
  interrupted turn delayed one review by about 10 minutes. Pilot runs now use 14400 seconds.
- The first dashboard was unreadable. [#728](https://github.com/onsager-ai/ostrom/pull/728) replaced
  it during the run with one card per ticket that shows the phase in words. The operator reached it
  over the tailnet through a forwarder that rewrites the Host header. The token check was unchanged.
- Duhem tickets were extended by hand: time +60 then +120 minutes, and tokens +500k then +1M.
  The extra turns came mostly from #725, #731 and #733.

## Next

Fix #731, #725, #732, #729 and #733 option 1 before the next real-repository run. Those five
account for nearly all the human work and extra tokens in this pilot. #733 options 2 and 3 and
#730 need the operator's decision.
