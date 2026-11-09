// Forward-only schema migrations for Ostrom's store. Each entry is applied once, in order, inside
// its own transaction, and recorded in `schema_version`. Never edit an entry that has shipped;
// append a new one.
//
// Ostrom stores only its own domain records (docs/direction.md, "Data model"). Sessions and
// transcripts stay in T3 Code and Semon; the store keeps their ids, never their content.

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
    db.exec("BEGIN IMMEDIATE");
    try {
      db.exec(sql);
      db.prepare("INSERT INTO schema_version (version, applied_at) VALUES (?, ?)").run(
        version,
        at.toISOString(),
      );
      db.exec("COMMIT");
    } catch (error) {
      db.exec("ROLLBACK");
      throw error;
    }
  }
  return migrations.length;
}
