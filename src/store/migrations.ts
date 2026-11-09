// Forward-only schema migrations for Ostrom's store. Each entry is applied once, in order, inside
// its own transaction, and recorded in `schema_version`. Never edit an entry that has shipped;
// append a new one.
//
// Ostrom stores only its own domain records (docs/direction.md, "Data model"). Sessions and
// transcripts stay in native session files and Semon; the store keeps their ids, never their content.

import type { DatabaseSync } from "node:sqlite";

export const migrations: readonly string[] = [
  // 1: the milestone 1 domain.
  `
  CREATE TABLE source (
    id     INTEGER PRIMARY KEY,
    kind   TEXT NOT NULL CHECK (kind IN ('github')),
    name   TEXT NOT NULL,                -- owner/repo
    cursor TEXT,                         -- opaque read position; NULL until first advanced
    UNIQUE (kind, name)
  ) STRICT;

  CREATE TABLE thread (
    id        INTEGER PRIMARY KEY,
    source_id INTEGER NOT NULL REFERENCES source (id),
    kind      TEXT NOT NULL CHECK (kind IN ('issue', 'pull_request')),
    number    INTEGER NOT NULL,
    url       TEXT NOT NULL,
    UNIQUE (source_id, number)
  ) STRICT;

  CREATE TABLE "trigger" (
    id              INTEGER PRIMARY KEY,
    source_id       INTEGER NOT NULL REFERENCES source (id),
    source_event_id TEXT NOT NULL,
    thread_id       INTEGER NOT NULL REFERENCES thread (id),
    kind            TEXT NOT NULL,       -- e.g. issue_comment, pull_request_review
    actor           TEXT NOT NULL,       -- metadata only, never identity
    url             TEXT NOT NULL,
    occurred_at     TEXT NOT NULL,
    received_at     TEXT NOT NULL,
    UNIQUE (source_id, source_event_id)
  ) STRICT;
  CREATE INDEX trigger_by_thread ON "trigger" (thread_id, occurred_at);
  CREATE TRIGGER trigger_is_immutable BEFORE UPDATE ON "trigger"
  BEGIN
    SELECT RAISE(ABORT, 'trigger rows are immutable');
  END;

  CREATE TABLE decision (
    id         INTEGER PRIMARY KEY,
    thread_id  INTEGER NOT NULL REFERENCES thread (id),
    verdict    TEXT NOT NULL CHECK (verdict IN ('accept', 'ignore', 'stop')),
    reason     TEXT NOT NULL CHECK (reason <> ''),
    decided_at TEXT NOT NULL
  ) STRICT;

  CREATE TABLE decision_trigger (
    decision_id INTEGER NOT NULL REFERENCES decision (id),
    trigger_id  INTEGER NOT NULL REFERENCES "trigger" (id),
    PRIMARY KEY (decision_id, trigger_id)
  ) STRICT, WITHOUT ROWID;

  CREATE TABLE ticket (
    id          INTEGER PRIMARY KEY,
    decision_id INTEGER NOT NULL UNIQUE REFERENCES decision (id),
    state       TEXT NOT NULL CHECK (state IN ('open', 'ended')),
    opened_at   TEXT NOT NULL,
    ended_at    TEXT,
    CHECK ((state = 'ended') = (ended_at IS NOT NULL))
  ) STRICT;
  CREATE TRIGGER ticket_needs_accept BEFORE INSERT ON ticket
  WHEN (SELECT verdict FROM decision WHERE id = NEW.decision_id) IS NOT 'accept'
  BEGIN
    SELECT RAISE(ABORT, 'only an accept decision creates a ticket');
  END;

  CREATE TABLE assignment (
    id           INTEGER PRIMARY KEY,
    ticket_id    INTEGER NOT NULL REFERENCES ticket (id),
    state        TEXT NOT NULL CHECK (state IN ('pending', 'running', 'ended')),
    t3_thread_id TEXT UNIQUE,            -- the T3 Code Worker thread; set when it starts running
    created_at   TEXT NOT NULL,
    started_at   TEXT,
    ended_at     TEXT,
    CHECK ((state = 'pending') = (t3_thread_id IS NULL)),
    CHECK ((state = 'pending') = (started_at IS NULL)),
    CHECK ((state = 'ended') = (ended_at IS NOT NULL))
  ) STRICT;
  CREATE INDEX assignment_by_ticket ON assignment (ticket_id);

  CREATE TABLE subscription (
    ticket_id     INTEGER NOT NULL REFERENCES ticket (id),
    thread_id     INTEGER NOT NULL REFERENCES thread (id),
    subscribed_at TEXT NOT NULL,
    PRIMARY KEY (ticket_id, thread_id)
  ) STRICT, WITHOUT ROWID;
  CREATE INDEX subscription_by_thread ON subscription (thread_id);

  CREATE TABLE outbox (
    id              TEXT PRIMARY KEY,    -- stable, chosen by the caller
    thread_id       INTEGER NOT NULL REFERENCES thread (id),
    body            TEXT NOT NULL,
    enqueued_at     TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT NOT NULL,
    last_error      TEXT,
    sent_at         TEXT
  ) STRICT;
  CREATE INDEX outbox_due ON outbox (next_attempt_at) WHERE sent_at IS NULL;
  `,

  // 2: Worker dispatch. An assignment whose T3 start failed ends from pending with the reason,
  // and a running assignment carries the last outcome observed on its T3 thread. SQLite cannot
  // alter CHECK constraints, so the table is rebuilt; nothing references it.
  `
  CREATE TABLE assignment_v2 (
    id           INTEGER PRIMARY KEY,
    ticket_id    INTEGER NOT NULL REFERENCES ticket (id),
    state        TEXT NOT NULL CHECK (state IN ('pending', 'running', 'ended')),
    t3_thread_id TEXT UNIQUE,            -- the T3 Code Worker thread; set when it starts running
    created_at   TEXT NOT NULL,
    started_at   TEXT,
    ended_at     TEXT,
    end_reason   TEXT CHECK (end_reason <> ''),
    outcome      TEXT CHECK (outcome IN ('waiting', 'failed', 'cancelled')),
    outcome_at   TEXT,
    CHECK ((t3_thread_id IS NULL) = (started_at IS NULL)),
    CHECK (state <> 'pending' OR t3_thread_id IS NULL),
    CHECK (state <> 'running' OR t3_thread_id IS NOT NULL),
    CHECK ((state = 'ended') = (ended_at IS NOT NULL)),
    -- An assignment that never started ends only with a reason.
    CHECK (state <> 'ended' OR t3_thread_id IS NOT NULL OR end_reason IS NOT NULL),
    CHECK (state = 'ended' OR end_reason IS NULL),
    CHECK ((outcome IS NULL) = (outcome_at IS NULL)),
    CHECK (outcome IS NULL OR t3_thread_id IS NOT NULL)
  ) STRICT;
  INSERT INTO assignment_v2 (id, ticket_id, state, t3_thread_id, created_at, started_at, ended_at)
    SELECT id, ticket_id, state, t3_thread_id, created_at, started_at, ended_at FROM assignment;
  DROP TABLE assignment;
  ALTER TABLE assignment_v2 RENAME TO assignment;
  CREATE INDEX assignment_by_ticket ON assignment (ticket_id);
  `,

  // 3: follow-up delivery. A trigger on a subscribed thread is sent to the ticket's Worker
  // without a decision; this records that it reached that Worker, and the T3 run it became, so a
  // replayed pass does not send it again.
  `
  CREATE TABLE delivery (
    trigger_id    INTEGER NOT NULL REFERENCES "trigger" (id),
    assignment_id INTEGER NOT NULL REFERENCES assignment (id),
    t3_run_id     TEXT NOT NULL CHECK (t3_run_id <> ''),
    delivered_at  TEXT NOT NULL,
    PRIMARY KEY (trigger_id, assignment_id)
  ) STRICT, WITHOUT ROWID;
  CREATE INDEX delivery_by_assignment ON delivery (assignment_id);
  CREATE TRIGGER delivery_needs_subscription BEFORE INSERT ON delivery
  WHEN NOT EXISTS (
    SELECT 1 FROM assignment
    JOIN subscription ON subscription.ticket_id = assignment.ticket_id
    JOIN "trigger" ON "trigger".thread_id = subscription.thread_id
    WHERE assignment.id = NEW.assignment_id AND "trigger".id = NEW.trigger_id
  )
  BEGIN
    SELECT RAISE(ABORT, 'a delivery needs the assignment''s ticket subscribed to the trigger''s thread');
  END;
  `,

  // 4: the Router. Each row is one T3 Code thread the Router ran in; the newest is the current
  // one, and a row is added when the previous thread is gone. Every batch the Router judges is
  // self-contained, so a new Router needs nothing from the old one's context.
  `
  CREATE TABLE router_thread (
    id           INTEGER PRIMARY KEY,
    t3_thread_id TEXT NOT NULL UNIQUE CHECK (t3_thread_id <> ''),
    started_at   TEXT NOT NULL
  ) STRICT;
  `,

  // 5: the Watchdog. One row per stall episode of a running assignment: opened when a signal
  // first shows the Worker stalled, cleared when no signal shows it any more. A row is flagged to
  // the Router once; its answer (wait or stop) is recorded on the row. At most one episode of an
  // assignment is open at a time.
  `
  CREATE TABLE stall (
    id            INTEGER PRIMARY KEY,
    assignment_id INTEGER NOT NULL REFERENCES assignment (id),
    semon_reason  TEXT CHECK (semon_reason <> ''),  -- Semon's stall_reason, when Semon listed it
    t3_reason     TEXT CHECK (t3_reason <> ''),     -- the T3 thread-state rule that matched
    detail        TEXT,                             -- e.g. the Worker's open question
    detected_at   TEXT NOT NULL,
    cleared_at    TEXT,
    action        TEXT CHECK (action IN ('wait', 'stop')),
    answered_at   TEXT,
    CHECK (semon_reason IS NOT NULL OR t3_reason IS NOT NULL),
    CHECK ((action IS NULL) = (answered_at IS NULL))
  ) STRICT;
  CREATE UNIQUE INDEX stall_open_episode ON stall (assignment_id) WHERE cleared_at IS NULL;
  `,

  // 6: Native Worker session metadata. Legacy T3 columns remain until the Router moves.
  `
  ALTER TABLE assignment ADD COLUMN harness TEXT CHECK (harness IN ('claude', 'codex'));
  ALTER TABLE assignment ADD COLUMN model TEXT;
  ALTER TABLE assignment ADD COLUMN session_id TEXT;
  ALTER TABLE assignment ADD COLUMN worktree TEXT;
  ALTER TABLE assignment ADD COLUMN branch TEXT;
  ALTER TABLE assignment ADD COLUMN machine TEXT;
  ALTER TABLE assignment ADD COLUMN turns INTEGER NOT NULL DEFAULT 0 CHECK (turns >= 0);
  ALTER TABLE assignment ADD COLUMN input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (input_tokens >= 0);
  ALTER TABLE assignment ADD COLUMN cached_input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cached_input_tokens >= 0);
  ALTER TABLE assignment ADD COLUMN output_tokens INTEGER NOT NULL DEFAULT 0 CHECK (output_tokens >= 0);
  `,

  // 7: A running assignment may use a native session instead of a T3 thread.
  `
  CREATE TABLE assignment_v7 (
    id INTEGER PRIMARY KEY,
    ticket_id INTEGER NOT NULL REFERENCES ticket(id),
    state TEXT NOT NULL CHECK (state IN ('pending', 'running', 'ended')),
    t3_thread_id TEXT UNIQUE,
    created_at TEXT NOT NULL,
    started_at TEXT,
    ended_at TEXT,
    end_reason TEXT CHECK (end_reason <> ''),
    outcome TEXT CHECK (outcome IN ('waiting', 'failed', 'cancelled')),
    outcome_at TEXT,
    harness TEXT CHECK (harness IN ('claude', 'codex')),
    model TEXT, session_id TEXT, worktree TEXT, branch TEXT, machine TEXT,
    turns INTEGER NOT NULL DEFAULT 0 CHECK (turns >= 0),
    input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    cached_input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cached_input_tokens >= 0),
    output_tokens INTEGER NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    CHECK (state <> 'pending' OR (t3_thread_id IS NULL AND session_id IS NULL AND started_at IS NULL)),
    CHECK (state <> 'running' OR (started_at IS NOT NULL AND (t3_thread_id IS NOT NULL OR session_id IS NOT NULL))),
    CHECK ((state = 'ended') = (ended_at IS NOT NULL)),
    CHECK (state <> 'ended' OR started_at IS NOT NULL OR end_reason IS NOT NULL),
    CHECK (state = 'ended' OR end_reason IS NULL),
    CHECK ((outcome IS NULL) = (outcome_at IS NULL)),
    CHECK (outcome IS NULL OR started_at IS NOT NULL)
  ) STRICT;
  INSERT INTO assignment_v7 SELECT * FROM assignment;
  DROP TRIGGER delivery_needs_subscription;
  DROP TABLE assignment;
  ALTER TABLE assignment_v7 RENAME TO assignment;
  CREATE INDEX assignment_by_ticket ON assignment (ticket_id);
  CREATE TRIGGER delivery_needs_subscription BEFORE INSERT ON delivery
  WHEN NOT EXISTS (
    SELECT 1 FROM assignment JOIN subscription ON subscription.ticket_id = assignment.ticket_id
    JOIN "trigger" ON "trigger".thread_id = subscription.thread_id
    WHERE assignment.id = NEW.assignment_id AND "trigger".id = NEW.trigger_id
  ) BEGIN
    SELECT RAISE(ABORT, 'a delivery needs the assignment''s ticket subscribed to the trigger''s thread');
  END;
  `,

  // 8: a native turn in progress and the interruptions a restart found.
  `
  ALTER TABLE assignment ADD COLUMN turn_started_at TEXT;
  ALTER TABLE assignment ADD COLUMN interruptions INTEGER NOT NULL DEFAULT 0 CHECK (interruptions >= 0);
  `,

  // 9: Router sessions are one-shot. T3 Worker columns remain for a later migration because
  // existing running T3 assignments have no native session ID to preserve through a clean rebuild.
  `
  DROP TABLE router_thread;
  ALTER TABLE delivery RENAME COLUMN t3_run_id TO turn_id;
  `,

  // 10: Every GitHub thread has at most one editable status card. The comment id is recorded
  // only after GitHub confirms the post, so a crash before that point is recovered by the marker.
  `
  ALTER TABLE thread ADD COLUMN status_card_comment_id INTEGER;
  `,

  // 11: The outbox sends comments, card edits and reactions. Existing reply rows become comments.
  `
  ALTER TABLE outbox ADD COLUMN kind TEXT NOT NULL DEFAULT 'comment'
    CHECK (kind IN ('comment', 'edit', 'reaction', 'label'));
  ALTER TABLE outbox ADD COLUMN target TEXT;
  ALTER TABLE outbox ADD COLUMN reaction TEXT;
  ALTER TABLE outbox ADD COLUMN blocking INTEGER NOT NULL DEFAULT 0 CHECK (blocking IN (0, 1));
  `,

  // 12: Submitted reviews are delivered as one Worker turn with their inline comments. Trigger
  // bodies and locations are immutable source data for that turn, never a transcript copy.
  `
  ALTER TABLE "trigger" ADD COLUMN review_id TEXT;
  ALTER TABLE "trigger" ADD COLUMN body TEXT;
  ALTER TABLE "trigger" ADD COLUMN path TEXT;
  ALTER TABLE "trigger" ADD COLUMN line INTEGER;
  CREATE INDEX trigger_review ON "trigger" (thread_id, review_id) WHERE review_id IS NOT NULL;
  `,

  // 13: Commands are consumed before ordinary follow-up delivery. A receipt is per ticket because
  // one subscribed thread can fan out to several tickets; it is also the durable instruction/info
  // classification for redirects from people outside the repository.
  `
  CREATE TABLE command_receipt (
    trigger_id   INTEGER NOT NULL REFERENCES "trigger" (id),
    ticket_id    INTEGER NOT NULL REFERENCES ticket (id),
    command      TEXT NOT NULL CHECK (command IN ('redirect', 'pause', 'resume', 'stop', 'forbidden', 'close_issue', 'close_pull_request')),
    instruction  INTEGER NOT NULL CHECK (instruction IN (0, 1)),
    handled_at   TEXT NOT NULL,
    PRIMARY KEY (trigger_id, ticket_id)
  ) STRICT, WITHOUT ROWID;
  CREATE INDEX command_receipt_by_ticket ON command_receipt (ticket_id, trigger_id);
  `,

  // 14: A paused ticket retains its Worker session but does not receive another follow-up until
  // it is resumed. Existing stores are rebuilt because SQLite cannot change a CHECK in place.
  `
  CREATE TABLE ticket_v14 (
    id          INTEGER PRIMARY KEY,
    decision_id INTEGER NOT NULL UNIQUE REFERENCES decision (id),
    state       TEXT NOT NULL CHECK (state IN ('open', 'paused', 'ended')),
    opened_at   TEXT NOT NULL,
    ended_at    TEXT,
    CHECK ((state = 'ended') = (ended_at IS NOT NULL))
  ) STRICT;
  INSERT INTO ticket_v14 SELECT * FROM ticket;
  DROP TABLE ticket;
  ALTER TABLE ticket_v14 RENAME TO ticket;
  CREATE TRIGGER ticket_needs_accept BEFORE INSERT ON ticket
  WHEN (SELECT verdict FROM decision WHERE id = NEW.decision_id) IS NOT 'accept'
  BEGIN
    SELECT RAISE(ABORT, 'only an accept decision creates a ticket');
  END;
  `,

  // 15: Ticket budgets are durable, per-ticket limits. Assignment counters remain the source of
  // turn and token usage; ticket-level queries sum every assignment for the ticket.
  `
  ALTER TABLE ticket ADD COLUMN budget_time_minutes INTEGER NOT NULL DEFAULT 480 CHECK (budget_time_minutes > 0);
  ALTER TABLE ticket ADD COLUMN budget_idle_minutes INTEGER NOT NULL DEFAULT 30 CHECK (budget_idle_minutes > 0);
  ALTER TABLE ticket ADD COLUMN budget_turns INTEGER NOT NULL DEFAULT 80 CHECK (budget_turns > 0);
  ALTER TABLE ticket ADD COLUMN budget_tokens INTEGER NOT NULL DEFAULT 1000000 CHECK (budget_tokens > 0);
  ALTER TABLE ticket ADD COLUMN budget_escalations INTEGER NOT NULL DEFAULT 0 CHECK (budget_escalations >= 0);
  ALTER TABLE ticket ADD COLUMN escalations INTEGER NOT NULL DEFAULT 0 CHECK (escalations >= 0);
  ALTER TABLE ticket ADD COLUMN pause_cause TEXT CHECK (pause_cause <> '');
  `,

  // 16: Idle budgets use the end of the latest completed native turn, not worker startup.
  `
  ALTER TABLE assignment ADD COLUMN last_activity_at TEXT;
  `,

  // 17: The stuck ladder is deterministic. It records two silent recovery attempts, then one
  // public escalation; a human reply clears the episode and permits ordinary follow-up delivery.
  `
  ALTER TABLE stall ADD COLUMN cause TEXT;
  ALTER TABLE stall ADD COLUMN recovery_attempts INTEGER NOT NULL DEFAULT 0 CHECK (recovery_attempts >= 0 AND recovery_attempts <= 2);
  ALTER TABLE stall ADD COLUMN public_at TEXT;
  ALTER TABLE stall ADD COLUMN cleared_by_human_at TEXT;
  `,

  // 18: The Watchdog detects the same turn error twice consecutively without retaining a transcript.
  `
  ALTER TABLE assignment ADD COLUMN last_error TEXT;
  ALTER TABLE assignment ADD COLUMN consecutive_errors INTEGER NOT NULL DEFAULT 0 CHECK (consecutive_errors >= 0);
  `,

  // 19: Recovery attempts belong to the Worker, not a transient Semon stall episode. A human
  // reply resets this counter; ending an assignment does too.
  `
  ALTER TABLE assignment ADD COLUMN stall_recovery_attempts INTEGER NOT NULL DEFAULT 0
    CHECK (stall_recovery_attempts >= 0 AND stall_recovery_attempts <= 2);
  `,

  // 20: An obsolete open issue-state trigger is consumed without affecting the subscribed ticket.
  `
  CREATE TABLE command_receipt_v20 (
    trigger_id   INTEGER NOT NULL REFERENCES "trigger" (id),
    ticket_id    INTEGER NOT NULL REFERENCES ticket (id),
    command      TEXT NOT NULL CHECK (command IN ('redirect', 'pause', 'resume', 'stop', 'forbidden', 'close_issue', 'close_pull_request', 'ignore_issue_state')),
    instruction  INTEGER NOT NULL CHECK (instruction IN (0, 1)),
    handled_at   TEXT NOT NULL,
    PRIMARY KEY (trigger_id, ticket_id)
  ) STRICT, WITHOUT ROWID;
  INSERT INTO command_receipt_v20 SELECT * FROM command_receipt;
  DROP TABLE command_receipt;
  ALTER TABLE command_receipt_v20 RENAME TO command_receipt;
  CREATE INDEX command_receipt_by_ticket ON command_receipt (ticket_id, trigger_id);
  `,
];

/** Applies every migration newer than the database's version. Returns the resulting version. */
export function migrate(db: DatabaseSync, at: Date = new Date()): number {
  db.exec(
    "CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL) STRICT",
  );
  const row = db.prepare("SELECT COALESCE(MAX(version), 0) AS version FROM schema_version").get();
  const current = Number(row?.["version"]);
  if (current > migrations.length) {
    throw new Error(
      `store schema version ${current} is newer than this code knows (${migrations.length})`,
    );
  }
  for (const [index, sql] of migrations.entries()) {
    const version = index + 1;
    if (version <= current) continue;
    // Rebuilding a referenced table requires foreign keys disabled outside the transaction.
    if (version === 7 || version === 9 || version === 14) db.exec("PRAGMA foreign_keys = OFF");
    db.exec("BEGIN IMMEDIATE");
    try {
      db.exec(sql);
      if ((version === 7 || version === 9 || version === 14) && db.prepare("PRAGMA foreign_key_check").get() !== undefined) {
        throw new Error("assignment migration broke a foreign key");
      }
      db.prepare("INSERT INTO schema_version (version, applied_at) VALUES (?, ?)").run(
        version,
        at.toISOString(),
      );
      db.exec("COMMIT");
    } catch (error) {
      db.exec("ROLLBACK");
      throw error;
    } finally {
      if (version === 7 || version === 9 || version === 14) db.exec("PRAGMA foreign_keys = ON");
    }
  }
  return migrations.length;
}
