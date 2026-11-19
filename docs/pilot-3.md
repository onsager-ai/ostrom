# Third real-repository pilot

Status: passed with manual help, 2026-11-19. All four issues became merged PRs on
`onsager-ai/duhem` and `onsager-ai/tolmap`. Ostrom merged three; the tolmap Manager merged the
fourth by hand because it was still a draft. The subjects were the pilot 2 fixes (#745 to #750)
and model selection step 4, outcome history (#760). The run found ten defects, listed below. All
times are UTC.

## Setup

- Host hp-code-2. One `ostromd` per repository, run from main at `b40f36d`, with the pilot 2
  stores, clones and launcher (`--max-seconds 14400`).
- Merge policy `approvals`, one approval from `tikazyq`. Budget: 60 minutes of turn time and 20
  turns per ticket; the operator raised tolmap ticket 9 to 150 minutes over three extensions.
- `ostrom models` as in pilot 2: luna (cost 1) and sol (cost 4), efforts low to high, objective
  `cost`. The Router ran on luna and now saw per-model outcome history.
- Each repository's Manager filed or chose the issues, updated its rules file and did every
  review. Two start comments also carried `@ostrom effort high`.

## Results

| Repository | Issue | PR | Model | Effort per turn | Turns | Active min | Fresh tokens | Opened | Merged |
|---|---|---|---|---|---|---|---|---|---|
| duhem | #703 docs headers | [#705](https://github.com/onsager-ai/duhem/pull/705) | luna | low, low, medium | 3 | 2 | 0.19M | 04:54 | 05:17 |
| duhem | #704 changelog placeholders | [#706](https://github.com/onsager-ai/duhem/pull/706) | **sol** | high, high | 2 | 5.2 | 0.10M | 04:56 | 05:18 |
| tolmap | #362 CLI docs | [#363](https://github.com/onsager-ai/tolmap/pull/363) | luna | medium, low | 2 | 9.2 | 0.16M | 04:55 | 05:15 |
| tolmap | #335 bindings before blending | [#364](https://github.com/onsager-ai/tolmap/pull/364) | **sol** | high (12 turns) | 12 | 109 | 2.36M | 05:05 | 10:31 (by hand) |

Three tickets closed within 25 minutes of starting. Ticket 9 on tolmap is the outlier: about 38
of its 109 active minutes were daemon downtime (#768), it waited 72 minutes on a red main that
wasn't its fault, and the work needed CI-only fixture regeneration that the App couldn't start
(#770).

What the fixes and model selection did:

- **Issue text reaches the Router (#745).** With start comments that said only "please take
  this", the Router still sized the work: luna for the two docs issues, sol for #704 ("requires
  careful parsing and test changes") and #335 ("changes extraction and clustering behavior and
  requires fixture regeneration").
- **Outcome history (#760).** On duhem#703 the Router's reason cited history ("the prior
  low-effort pass missed a consumer leaf") and raised a review turn to medium.
- **Start-comment overrides (#749).** `effort high` applied from the first turn on #704 and #335.
  The reply wording is wrong and duplicated (#764).
- **Mention required (#746), approval commentary (#747), immediate control replies (#748),
  pending override on the card (#750).** All held. No status comment started a ticket.

## Defects found

| Issue | What happened |
|---|---|
| [#762](https://github.com/onsager-ai/ostrom/issues/762) | A failed check on the Worker's PR head doesn't wake the Worker; duhem#706 waited for a person to point at the compile error |
| [#763](https://github.com/onsager-ai/ostrom/issues/763) | duhem#705's body had literal `\n\n`, so "Closes #703" didn't register and the issue stayed open |
| [#764](https://github.com/onsager-ai/ostrom/issues/764) | A start-comment override says "applies to the next turn", and a second comment repeats the card |
| [#765](https://github.com/onsager-ai/ostrom/issues/765) | Ostrom's own "PR is open" notice restarted the merge quiet window |
| [#766](https://github.com/onsager-ai/ostrom/issues/766) | A turn's batched reply posted after the next turn started (tolmap#364) or after the PR opened (duhem#704) |
| [#767](https://github.com/onsager-ai/ostrom/issues/767) | After a restart, the replayed turn answered a review whose fix CI had already checked |
| [#768](https://github.com/onsager-ai/ostrom/issues/768) | A thermal stop's 38 minutes of downtime counted as turn time; the Codex turn was left unterminated; a budget pause cancelled a running turn |
| [#769](https://github.com/onsager-ai/ostrom/issues/769) | tolmap#364 stayed a draft, and the merge refusal said only "not clean" for 50 minutes |
| [#770](https://github.com/onsager-ai/ostrom/issues/770) | The App gets HTTP 403 on `workflow_dispatch` |
| [#771](https://github.com/onsager-ai/ostrom/issues/771) | A Worker edited a file the rules forbade and didn't say so |

Minor: a trivial review fix ran at medium where low would do, and the pilot's log watches missed
the thermal stop and the budget pauses, which aren't logged.

## Manual steps

- tolmap Manager: filed the tickets, fixed tolmap's red main (#365, runner pinned), dispatched the
  full-fixture CI run twice and committed the bindings from it twice, made two owner decisions on
  the rules, and marked #364 ready and merged it (97cda4c).
- duhem Manager: closed #703 by hand.
- Operator: three budget extensions, two daemon restarts, one rules-file edit.

## Next

#762 comes first: CI is the Workers' only compiler on these hosts, and escalation (model-selection
step 5) counts consecutive red heads, so it needs this wake. #769 and #763 each took a merge or an
issue close out of Ostrom's hands, and both are small. #768 matters for any long ticket on a host
with a thermal guard. #770 is an operator decision about the App's permissions.
