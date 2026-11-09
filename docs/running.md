# Running ostromd

How an operator runs the milestone 1 loop (`docs/direction.md`) on one repository, and how to read
the evidence the pass criterion asks for. Installing ostromd as a service is not covered.

## Requirements

- Node `>=24.22.2` (see `.node-version`); ostromd runs from source, `node src/ostromd/main.ts`.
- `gh`, logged in as the operator. Replies go out under that login, marked so ostromd drops them
  when it reads them back.
- `t3` at the pinned version (`src/t3/pin.ts`: v0.1.7), with T3 Code running and the repository
  added as a T3 project.
- `semon`, for the Watchdog's `semon query stalls` read (`docs/watchdog.md`).

## Environment

Every variable is required, and none has a default, so no Worker or Router runs on a model nobody
chose. `tick` and `run` check all of them before any pass runs.

| Variable | Meaning |
|---|---|
| `OSTROM_REPO` | `owner/name` of the one repository to watch |
| `OSTROM_ALLOWLIST` | Comma-separated GitHub logins who may start work |
| `OSTROM_DB` | Path to the SQLite store; created on first use |
| `OSTROM_T3_PROJECT` | T3 Code project id, or a path inside the project; Workers and the Router run there |
| `OSTROM_WORKER_BASE` | Branch each Worker's worktree starts from and its PR targets |
| `OSTROM_WORKER_PROVIDER` | T3 provider instance id for Workers |
| `OSTROM_WORKER_MODEL` | Model slug for Workers |
| `OSTROM_ROUTER_PROVIDER` | T3 provider instance id for the Router thread |
| `OSTROM_ROUTER_MODEL` | Model slug for the Router thread |
| `OSTROM_STALL_MINUTES` | Whole minutes without progress before the Watchdog calls a Worker stalled |

With cheap models on both sides:

```sh
export OSTROM_REPO=onsager-ai/ostrom
export OSTROM_ALLOWLIST=<your-login>
export OSTROM_DB="$HOME/.local/state/ostrom/ostrom.sqlite"
export OSTROM_T3_PROJECT=<path inside the T3 project>
export OSTROM_WORKER_BASE=main
export OSTROM_WORKER_PROVIDER=claudeAgent OSTROM_WORKER_MODEL=claude-haiku-5-5
export OSTROM_ROUTER_PROVIDER=claudeAgent OSTROM_ROUTER_MODEL=claude-haiku-5-5
export OSTROM_STALL_MINUTES=20
```

The directory holding `OSTROM_DB` must exist.

## Running the loop

```sh
node src/ostromd/main.ts run --interval 60
```

`run` repeats `tick` and waits the interval after each. A tick runs every pass once, in order:
poll → observe → route → watchdog → route-candidates → send. A failed pass is reported on stderr
and the rest still run. SIGINT or SIGTERM stops the loop after the current tick, and the exit code
is the last tick's. Each pass is also its own command (`node src/ostromd/main.ts` with no command
prints them all); `tick` runs one round and exits non-zero if any pass failed.

The first poll on a new store sets a baseline at the current time and reads back only a short
overlap, so the repository's older comments never reach the Router. Comment after the loop starts.

The `send` pass is the only one that posts ostromd's replies to GitHub. To hold them back, run the
passes yourself and leave out `send`; replies wait in the outbox. Workers still push branches and
open pull requests.

Workers never ask interactive questions (the brief forbids it): at the T3 pin a follow-up is
queued behind an unanswered question instead of answering it. A Worker that needs an answer puts
the question in its PR description and ends its turn; the reply on GitHub reaches it as a
follow-up.

## Evidence

The pass criterion is an event that becomes a merged PR, review follow-up included, with no human
relaying anything. The evidence is the store's records for that ticket, joined to the T3 thread
ids and the GitHub URLs:

```sh
node src/ostromd/main.ts evidence               # every ticket, one JSON line each
node src/ostromd/main.ts evidence --ticket 3    # one ticket; exits non-zero if it does not exist
```

Each line carries, for one ticket:

- `decision` and `triggers`: the Router's accept and the comments it accepted, with their URLs.
- `assignments`: each Worker, with its T3 thread id and its branch (`ostrom/ticket-<t>-<a>`, also
  the key Semon knows the session by).
- `subscriptions`: the accepted issue and the Worker's PR, with their URLs.
- `followUps`: comments on a subscribed thread delivered to a Worker, with the T3 run each became.
- `stalls`: Watchdog episodes and the Router's answer to each.
- `replies`: outbox rows owed for the ticket (`decision-…`, `outcome-a…`, `stall-…`), with the
  thread they go to and whether they were sent.

A passing run shows an accept, an assignment whose `t3ThreadId` is the Worker, a pull request in
`subscriptions`, at least one review comment in `followUps`, and a sent `outcome-a<id>-pr<n>-merged`
reply. `t3 thread show <t3ThreadId> --json` and the GitHub URLs lead to the rest.

`evidence` runs this query; it works in any SQLite shell too. `:ticket` picks one ticket; left
unbound (NULL), it lists all of them.

```sql
SELECT json_object(
  'ticketId', ticket.id,
  'state', ticket.state,
  'openedAt', ticket.opened_at,
  'endedAt', ticket.ended_at,
  'decision', json_object(
    'id', decision.id,
    'verdict', decision.verdict,
    'reason', decision.reason,
    'decidedAt', decision.decided_at,
    'thread', json((SELECT json_object('kind', kind, 'number', number, 'url', url)
                    FROM thread WHERE thread.id = decision.thread_id))
  ),
  'triggers', json((SELECT json_group_array(json_object(
      'id', id, 'kind', kind, 'actor', actor, 'url', url, 'occurredAt', occurred_at))
    FROM (SELECT t.* FROM decision_trigger dt JOIN "trigger" t ON t.id = dt.trigger_id
          WHERE dt.decision_id = decision.id ORDER BY t.occurred_at, t.id))),
  'assignments', json((SELECT json_group_array(json_object(
      'id', id, 'state', state, 't3ThreadId', t3_thread_id,
      'branch', 'ostrom/ticket-' || ticket_id || '-' || id,
      'startedAt', started_at, 'endedAt', ended_at, 'endReason', end_reason, 'outcome', outcome))
    FROM (SELECT * FROM assignment WHERE ticket_id = ticket.id ORDER BY id))),
  'subscriptions', json((SELECT json_group_array(json_object(
      'kind', kind, 'number', number, 'url', url, 'subscribedAt', subscribed_at))
    FROM (SELECT th.kind, th.number, th.url, s.subscribed_at
          FROM subscription s JOIN thread th ON th.id = s.thread_id
          WHERE s.ticket_id = ticket.id ORDER BY s.subscribed_at, th.number))),
  'followUps', json((SELECT json_group_array(json_object(
      'triggerUrl', url, 'actor', actor, 'assignmentId', assignment_id,
      't3ThreadId', t3_thread_id, 't3RunId', t3_run_id, 'deliveredAt', delivered_at))
    FROM (SELECT t.url, t.actor, d.assignment_id, a.t3_thread_id, d.t3_run_id, d.delivered_at
          FROM delivery d JOIN assignment a ON a.id = d.assignment_id
          JOIN "trigger" t ON t.id = d.trigger_id
          WHERE a.ticket_id = ticket.id ORDER BY d.delivered_at, t.id))),
  'stalls', json((SELECT json_group_array(json_object(
      'id', id, 'assignmentId', assignment_id, 'semonReason', semon_reason, 't3Reason', t3_reason,
      'detectedAt', detected_at, 'clearedAt', cleared_at, 'action', action, 'answeredAt', answered_at))
    FROM (SELECT st.* FROM stall st JOIN assignment a ON a.id = st.assignment_id
          WHERE a.ticket_id = ticket.id ORDER BY st.id))),
  'replies', json((SELECT json_group_array(json_object(
      'id', id, 'threadUrl', url, 'enqueuedAt', enqueued_at, 'sentAt', sent_at,
      'attempts', attempts, 'lastError', last_error))
    FROM (SELECT o.id, th.url, o.enqueued_at, o.sent_at, o.attempts, o.last_error
          FROM outbox o JOIN thread th ON th.id = o.thread_id
          WHERE o.id = 'decision-' || ticket.decision_id
             OR o.id IN (SELECT 'stall-' || st.id FROM stall st JOIN assignment a ON a.id = st.assignment_id
                         WHERE a.ticket_id = ticket.id)
             OR EXISTS (SELECT 1 FROM assignment a WHERE a.ticket_id = ticket.id
                        AND substr(o.id, 1, length('outcome-a' || a.id || '-')) = 'outcome-a' || a.id || '-')
          ORDER BY o.enqueued_at, o.id)))
) AS evidence
FROM ticket JOIN decision ON decision.id = ticket.decision_id
WHERE :ticket IS NULL OR ticket.id = :ticket
ORDER BY ticket.id;
```
