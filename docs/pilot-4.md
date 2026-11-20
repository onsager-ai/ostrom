# Fourth real-repository pilot

Status: partly passed, 2026-11-19. Five tickets on `onsager-ai/duhem` and `onsager-ai/tolmap`.
Ostrom merged both tolmap PRs by itself. On duhem, one PR is green and waits for review, one
ticket is unfinished and one was stopped by its Manager. The duhem Manager was blocked on an
operator prompt from 16:03, so the duhem half is recorded as it stood then. The subjects were the
pilot 3 fixes (#762 to #771). The run found five defects; three were fixed and deployed during the
pilot. All times are UTC.

## Setup

- Host hp-code-2. One `ostromd` per repository, run from main at `813bbef` until 17:20, then at
  `a14b826` (with #784, #782 and #783). The daemons now run from a detached checkout of
  `origin/main` (`OSTROM_CHECKOUT`), not the operator's main checkout.
- Same stores, clones, launcher, policy (`approvals`, one approval from `tikazyq`), budgets (60
  minutes and 20 turns per ticket) and models (luna cost 1, sol cost 4) as pilot 3.
- The Ostrom App's installation gained `actions: write` before the pilot (#770).
- Semon on the host was upgraded to a build with the session-annotation reader (semon#709), so
  each Worker session shows its Ostrom repository, ticket, issue and PR.

## Results

| Repository | Issue | PR | Model | Effort per turn | Turns | Active min | Fresh tokens | Outcome |
|---|---|---|---|---|---|---|---|---|
| duhem | #392 schema packaging | none | luna | medium, medium, low | 3 | 3.8 | 0.23M | Unfinished: blocked, then redirected; branch pushed, no PR |
| duhem | #696 README screenshot | none | luna | medium | 1 (+stop) | 3.1 | 0.25M | Blocked: no duhem binary with #695; stopped by its Manager |
| duhem | #379 shared catalogs | [#709](https://github.com/onsager-ai/duhem/pull/709) | **sol** | medium, medium | 2 | 11.0 | 0.17M | Green after an automatic CI-fix turn; awaiting review |
| tolmap | #46 merge routes | [#368](https://github.com/onsager-ai/tolmap/pull/368) | luna | medium ×3 | 3 | 28.0 | 0.65M | Merged by Ostrom 16:56 |
| tolmap | #366 coverage docs | [#367](https://github.com/onsager-ai/tolmap/pull/367) | luna | medium, low, low | 3 | 11.5 | 0.26M | Merged by Ostrom 16:27 |

What the pilot 3 fixes did:

- **Red CI wakes the Worker (#762).** duhem#709's first head failed "duhem run (1/2)" at 14:47. Ostrom
  queued a CI-fix turn 14 seconds later with the job name and log link, and the fix head went green.
  Nobody commented.
- **Workers dispatch CI themselves (#770).** The tolmap#368 Worker started the nine-fixture run
  three times without a 403.
- **Ready PRs, quiet window, approvals (#765, #769).** All three PRs opened ready. tolmap#367
  merged six minutes after a one-sentence approval, with no Worker wake.
- **Model choice.** The Router chose sol only for duhem#379, a schema change with an open design
  question. It chose luna for tolmap#46 because luna has "the strongest merge history for this
  repository". The tolmap Manager thought that was low for a pipeline change plus a measurement
  write-up, but luna got the code right the first time.
- **Semon.** Every Worker session appeared in `semon sessions` with its annotation. The title is
  filled when Ostrom first sees the PR.

## Defects found

| Issue | What happened | State |
|---|---|---|
| [#784](https://github.com/onsager-ai/ostrom/issues/784) | Ticket branches were cut from the clone's local `main`, which was never fetched: tolmap from 2026-11-18 (missing #359 to #365), duhem from 2026-11-17. tolmap#368's fixture runs failed on a runner pin main had already fixed, and #367 documented 6 of 8 fields | Fixed, #786 |
| [#782](https://github.com/onsager-ai/ostrom/issues/782) | Two duhem Workers stopped blocked by the no-local-build rule. Ostrom recorded an empty result, posted nothing and kept the card on Working for 85 minutes | Fixed, #787 |
| [#783](https://github.com/onsager-ai/ostrom/issues/783) | The new ticket on duhem#392 received a 2026-11-17 comment from pilot 1's ticket on the same issue | Fixed, #785 |
| [#788](https://github.com/onsager-ai/ostrom/issues/788) | Nothing wakes a Worker when a run it dispatched finishes. The tolmap#368 Worker didn't report two failed runs, then ended a turn with a run in progress; the tolmap Manager relayed the counts | Fixed, #790 |
| — | duhem#392: CI runs only on pull requests, but the Manager's redirect asked the Worker to take the regenerated schema from CI before opening one. The Worker pushed the branch and stopped. A CI-only regeneration step needs the PR open first | Brief/redirect wording, not filed |

Operational notes:

- The no-local-build house rule blocks work that needs a generated file or a fresh binary. Two of
  three duhem tickets hit it. Generated-file changes need a CI path the Worker can use (open the PR,
  then take the output from CI).
- Workers can't set `core.hooksPath` because the shared clone's `.git/config` is read-only in the
  Worker sandbox.
- The tolmap daemon's graceful stop took longer than its 10-second limit once and exited non-zero
  (#768's bound working).

## Manual steps

- tolmap Manager: picked the tickets, diagnosed the stale base and the failed dispatches, read the
  finished on-demand run and relayed its counts, checked that a red profile job was unrelated.
- duhem Manager: picked the tickets, redirected #392, stopped #696.
- Operator: one daemon restart to deploy #782, #783 and #784.

## Next

#788 first: on-demand runs are now the main way Workers get results that PR checks don't
produce. duhem#709 still needs its review. The no-local-build rule plus generated files needs a
documented CI route in the Worker brief before duhem gets more schema work.

## After the write-up (2026-11-19 to 2026-11-20)

- **duhem#709 (#379).** Ostrom woke the Worker after Duhem PM's review on its own. The Worker
  pushed `b46a334` and `8ae19b4` at about 02:09 UTC, replied on the PR and CI went green. The PR
  then waited about nine hours for a re-review, because nothing tells the reviewer that a fix
  landed. It is still open at pilot close.
- **duhem#392.** Unfinished: branch pushed, no PR (see Defects found).
- **Thermal stops.** The thermal gate stopped both daemons twice: at 90 °C (02:20 UTC) and at
  86 °C (02:27 UTC). Both times the heat came from Semon's first `semon push --watch` sync for the
  new phone viewer, not from Ostrom. No Worker turn was running either time. Semon's services
  now run at nice 19 with CPU caps (20% push, 30% receive), and the daemons ran cool afterwards.
- **Session links.** The status card and `ostrom show` link each Worker session to the
  hp-code-2 Semon viewer when `OSTROM_SEMON_URL` is set (#792), through Semon's
  `/session/by-native/<harness>/<id>` redirect. Sessions that are still running do not load yet;
  Semon will fix that in its warehouse phase 3.
- **Sol on gpt-6.2.** The pinned Codex SDK 0.167.0 bundled a CLI whose `model/list` lacked
  gpt-6.2-sol. #794 moved the pin to 0.170.0. Both pilot stores now allow `codex/gpt-6.2-sol` with
  the old sol settings, and `codex/gpt-6.1-sol` is removed.
- **Restarts.** Besides the thermal restarts, the daemons stopped at their 4-hour limit once and
  were stopped by hand for the Magpie upgrade to 0.1.4591.

Still open: a way to tell the reviewer a fix landed (or to re-request review), and the
generated-files CI route in the Worker brief.
