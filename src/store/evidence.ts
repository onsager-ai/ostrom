// The pass criterion's evidence (docs/direction.md, milestone 1): the store's records for each
// ticket, joined to the T3 thread ids and GitHub URLs they point at. `ostromd evidence` runs this
// query, and docs/running.md quotes it verbatim so an operator can run it in any SQLite shell.
//
// One row per ticket, one JSON object per row. `:ticket` selects one ticket; NULL (or unbound, in
// the sqlite3 shell) lists them all. Outbox rows are matched by the ids the daemon gives them:
// `decision-<decision>`, `outcome-a<assignment>-...` and `stall-<stall>`.
//
// Subquery values are wrapped in json() so they nest as JSON, not as strings; each list is built
// from an ordered inner select.

export const EVIDENCE_SQL = `SELECT json_object(
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
ORDER BY ticket.id;`;
