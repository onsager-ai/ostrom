# Second real-repository pilot

Status: passed, 2026-11-17. All six issues became PRs that `ostrom-agent[bot]` authored and merged,
on `onsager-ai/duhem` and `onsager-ai/tolmap`. The main subject was model selection
([model-selection.md](model-selection.md) steps 1 to 3). The run found eight defects, listed
below. All times are UTC.

## Setup

- Host hp-code-2. One `ostromd` per repository, run from main at `5a3f676`, with the pilot 1 stores,
  clones, `XDG_RUNTIME_DIR`s and launcher (`--max-seconds 14400`).
- Merge policy `approvals`, one approval from `tikazyq`. Budget: 60 minutes of turn time and 20
  turns per ticket. No ticket came near either limit.
- `ostrom models`: `codex/gpt-6-luna` (cost 1) and `codex/gpt-6.1-sol` (cost 4) allowed, both
  limited to efforts low, medium and high, with objective `cost`. The Router ran on luna.
- Each repository's Manager updated its rules file before the tickets started, chose the issues
  and did every review. Ostrom started each ticket from an `@ostrom` comment on the issue.

## Results

| Repository | Issue | PR | Model | Effort per turn | Turns | Active min | Fresh tokens | Opened | Merged |
|---|---|---|---|---|---|---|---|---|---|
| duhem | #697 docs | [#700](https://github.com/onsager-ai/duhem/pull/700) | luna | medium, low | 2 | 3.1 | 0.10M | 17:10 | 17:30 |
| duhem | #698 CLI hint | [#701](https://github.com/onsager-ai/duhem/pull/701) | luna | medium | 1 | 3.3 | 0.04M | 17:11 | 17:42 |
| duhem | #699 dashboard state | [#702](https://github.com/onsager-ai/duhem/pull/702) | luna | medium, low | 2 | 9.6 | 0.18M | 17:11 | 17:34 |
| tolmap | #358 positional arg | [#359](https://github.com/onsager-ai/tolmap/pull/359) | luna | medium, low, medium, medium, medium | 5 | 6.4 | 0.43M | 17:11 | 18:23 |
| tolmap | #142 fd-relative removal | [#360](https://github.com/onsager-ai/tolmap/pull/360) | **sol** | medium, medium, medium, low | 4 | 13.2 | 0.36M | 17:13 | 17:51 |
| tolmap | #333 hidden `.json` | [#361](https://github.com/onsager-ai/tolmap/pull/361) | luna | medium, low, high | 3 | 5.4 | 0.24M | 18:57 | 19:38 |

Compared with pilot 1, the duhem tickets took 1 to 2 turns instead of 11 to 16, and 0.04M to 0.18M
fresh tokens instead of 0.7M to 2.1M. The pilot 1 fixes (#725, #729, #731, #732, #733 option 1)
account for most of that. Active minutes is the new time budget's count: the sum of turn durations.

What the model selection did:

- **Model.** When the start comment described the work, the picks matched what the Managers
  expected. On #142 the Router chose sol because "this requires careful handling of file
  descriptors, symlink races and removal counts". It chose luna for #358 and #333 because each
  was "a focused change limited to `src/main.rs` and its tests". On duhem the start comments only
  said "please take this", and the Router doesn't see the issue text (#745), so every ticket got
  luna, including #699. Luna still finished #699 in one review round.
- **Effort.** Medium on first turns and low on narrow review fixes held up. One low turn fixed both
  review points on duhem#702. Luna at medium on tolmap#359 got the code right but took three CI
  rounds to match clap's real help output. The tolmap Manager thinks a rule to run the help output
  before asserting on it would help more than higher effort.
- **Overrides.** `@ostrom effort high` posted on its own was recorded at once and applied on the
  next turn (the last turn on tolmap#361 ran at high). Posted in the start comment, it was dropped
  (#749). On duhem#698 no further turn ran, and the card gave no sign of the pending override
  (#750).
- **Automatic conflict turn.** The #742 path ran for real: #359's merge left #361 conflicting, and
  Ostrom sent the idle Worker a merge-main turn at low effort without anyone asking.

## Defects found

| Issue | What happened |
|---|---|
| [#745](https://github.com/onsager-ai/ostrom/issues/745) | When a comment starts a ticket, the Router sees only that comment, not the issue text, so it can't size the work |
| [#746](https://github.com/onsager-ai/ostrom/issues/746) | Any allowlisted comment reaches the Router. On startup it caught up on two tolmap#337 status comments and started an unrequested ticket, which the operator stopped after 5 minutes |
| [#747](https://github.com/onsager-ai/ostrom/issues/747) | An approval with one sentence of commentary still wakes the Worker, which restarts the quiet window (duhem#700, tolmap#360 and #361) |
| [#748](https://github.com/onsager-ai/ostrom/issues/748) | Replies to control commands wait for the batch window, so a command looks ignored for minutes |
| [#749](https://github.com/onsager-ai/ostrom/issues/749) | Commands in the comment that starts a ticket are dropped silently |
| [#750](https://github.com/onsager-ai/ostrom/issues/750) | The card doesn't show a pending model or effort override |
| [#751](https://github.com/onsager-ai/ostrom/issues/751) | A macOS CI job cancelled before it got a runner showed only as "not clean"; the Manager re-ran it by hand |
| [#752](https://github.com/onsager-ai/ostrom/issues/752) | PR bodies had both "Refs #N" and "Closes #N", and one edit dropped "Closes #142" |

Operational notes:

- The first tolmap ticket recorded "Missing or invalid Router model choice null". The operator
  allowed the models a few seconds after that Router turn had started. Every later ticket had a
  real choice and reason.
- `ostrom models` lists every discovered model first, so the two allowed ones were hard to find
  in a long list.
- Rules files are read when `ostromd` starts, so changing them needs a daemon restart.

## Next

#745, #746 and #749 come first. Without #745 the model choice can't be judged on most tickets, and
#746 can start unwanted work. #747, #748 and #750 are small and remove most of the remaining
reviewer friction. Model-selection step 4 (outcome history) can then use this pilot's `turn` rows.
