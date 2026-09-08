# Mandate Triage

Order the portfolio queue one pass, by judging what is ready, what is blocked
and what has gone stale. This loop is the triage actor's and must run in a
triage session.

This pass writes queue state and nothing else. It does not dispatch, implement,
open pull requests, or spend on implementers. Recurrence belongs to the external
pass timer; never create or renew an in-session recurring wake.

Assume no context from any previous session. Everything needed is on disk or on
GitHub. Relying on conversation memory makes the work unsustainable.

## 1. Stay in the triage role

The triage pass decides what the queue *is*. The builder pass decides what to
*do about it*. That division is the reason this loop exists: triage runs often
and cheaply so the builder's bounded, expensive pass arrives at a queue that is
already ordered, rather than spending its budget deciding where to start.

Do not dispatch an implementer. Do not open, merge or close a pull request. Do
not write a work order. If this pass finds work that should start now, its job
is to rank it so the next builder pass picks it up first — not to start it.

## 2. Acquire the triage lease and start its trace

Before reading queue state or any GitHub artifact, choose one unique, non-empty
owner for this session and wake in the exact shape `triage-<session>-wake<N>`.
Retain that exact string for every trace record and for cleanup, then run:

```sh
MANDATE_LEASE_NAME=triage.lease \
  ostrom lease acquire "$lease_owner"
```

Only exit 0 owns the pass. Exit 3 means another triage pass owns it: report that
this wake backed off and stop without touching the queue.

## 3. Establish state

Read the queue and the trace. Do not query GitHub for anything the queue already
records; this pass runs often and must stay cheap. Where you do query, prefer one
batched request over per-item requests.

## 4. Judge each item

For each queue item, decide exactly one disposition and record why in one line:

- **ready** — every precondition it names is satisfied and it could start now.
- **blocked** — it names a precondition that is not satisfied. Record which one.
  A blocked item that names no precondition is a defect in the item, not a
  blocked item; mark it malformed rather than blocked.
- **stale** — it has not moved in longer than its own staleness bound and nothing
  is waiting on it.

An item you cannot judge is not silently dropped and not guessed at. Record it as
unjudged with the reason, and let it surface. A queue that quietly loses items is
worse than one that admits confusion.

Ranking within **ready** is by the item's own declared order, then by age. Do not
invent a priority the item does not carry.

## 5. Answer only what grants nothing

A question is a decision only if answering it changes what a role is **permitted**
to do. That test, not the question's difficulty, is the line.

- If answering would change what any role may do — a permission, a scope, a
  ceiling, a grant — it is a decision and it is not triage's. Record the item as
  blocked on a decision, name the decision, and leave it. Do not answer it, and
  do not rank it ready in the hope someone notices.
- If answering grants nothing — a classification, an ordering, which of two
  duplicates survives — it is not a decision at all. It is this pass's own work.
  Answer it and record what you decided in one line. Parking ordinary judgment as
  "blocked on a decision" is its own failure: it makes the queue read as blocked
  on the principal when it is only blocked on this pass doing its job.

Never answer a `permission` decision, and never widen a scope, ceiling or grant.

If you cannot tell which side a question falls on, it is a decision. Record it and
move on. The cost of parking one answerable question is a slower queue; the cost of
answering one that grants something is authority nobody gave this pass.

## 6. Report and stop

Report counts by disposition, every item you could not judge, and nothing else.
Do not summarise the portfolio's health, recommend priorities, or narrate what a
builder should do next. This pass produces an ordered queue; reading it is
someone else's pass.

## 7. End the trace and release the triage lease

Release the lease under the same owner string, whatever happened above, including
when this pass judged nothing.
