// Ostrom's one SQLite store: typed access to its own domain records, and nothing else.
// See docs/direction.md, "Data model". Only the operations milestone 1 needs live here.

import { DatabaseSync, type SQLInputValue } from "node:sqlite";
import { migrate } from "./migrations.ts";

export type SourceKind = "github";
export type ThreadKind = "issue" | "pull_request";
export type Verdict = "accept" | "ignore" | "stop";
export type TicketState = "open" | "ended";
export type AssignmentState = "pending" | "running" | "ended";

export interface Source {
  id: number;
  kind: SourceKind;
  name: string;
  cursor: string | null;
}

export interface NewThread {
  sourceId: number;
  kind: ThreadKind;
  number: number;
  url: string;
}

export interface Thread extends NewThread {
  id: number;
}

export interface NewTrigger {
  sourceId: number;
  sourceEventId: string;
  threadId: number;
  kind: string;
  actor: string;
  url: string;
  occurredAt: Date;
}

export interface NewDecision {
  threadId: number;
  triggerIds: readonly number[];
  verdict: Verdict;
  reason: string;
}

export interface Ticket {
  id: number;
  decisionId: number;
  state: TicketState;
  openedAt: string;
  endedAt: string | null;
}

export interface Assignment {
  id: number;
  ticketId: number;
  state: AssignmentState;
  t3ThreadId: string | null;
  createdAt: string;
  startedAt: string | null;
  endedAt: string | null;
}

export interface NewReply {
  id: string;
  threadId: number;
  body: string;
}

export interface Reply {
  id: string;
  threadId: number;
  body: string;
  enqueuedAt: string;
  attempts: number;
  nextAttemptAt: string;
  lastError: string | null;
  sentAt: string | null;
}

/** An insert that may have hit an existing row. */
export interface Inserted {
  id: number;
  duplicate: boolean;
}

/** A state change the record's current state does not allow. */
export class IllegalTransition extends Error {
  override name = "IllegalTransition";
}

export interface StoreOptions {
  /** The clock used for every timestamp the store writes and for due-reply checks. */
  now?: () => Date;
}

type Row = Record<string, unknown>;

export class Store {
  readonly #db: DatabaseSync;
  readonly #now: () => Date;

  constructor(db: DatabaseSync, options: StoreOptions = {}) {
    this.#db = db;
    this.#now = options.now ?? (() => new Date());
    migrate(db, this.#now());
  }

  /** Opens (creating if needed) the store at `path`; `:memory:` gives a throwaway store. */
  static open(path: string, options: StoreOptions = {}): Store {
    return new Store(new DatabaseSync(path, { enableForeignKeyConstraints: true }), options);
  }

  close(): void {
    this.#db.close();
  }

  // --- sources and threads -------------------------------------------------------------------

  /** Returns the source, creating it with no cursor on first use. */
  ensureSource(kind: SourceKind, name: string): Source {
    this.#run("INSERT INTO source (kind, name) VALUES (?, ?) ON CONFLICT DO NOTHING", kind, name);
    const row = this.#get("SELECT * FROM source WHERE kind = ? AND name = ?", kind, name)!;
    return toSource(row);
  }

  readCursor(sourceId: number): string | null {
    const row = this.#get("SELECT cursor FROM source WHERE id = ?", sourceId);
    if (row === undefined) throw new Error(`no source ${sourceId}`);
    return row["cursor"] as string | null;
  }

  advanceCursor(sourceId: number, cursor: string): void {
    const { changes } = this.#run("UPDATE source SET cursor = ? WHERE id = ?", cursor, sourceId);
    if (Number(changes) !== 1) throw new Error(`no source ${sourceId}`);
  }

  /** Returns the thread's id, creating the thread on first sight. */
  ensureThread(thread: NewThread): number {
    this.#run(
      "INSERT INTO thread (source_id, kind, number, url) VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
      thread.sourceId,
      thread.kind,
      thread.number,
      thread.url,
    );
    const row = this.#get(
      "SELECT id, kind FROM thread WHERE source_id = ? AND number = ?",
      thread.sourceId,
      thread.number,
    )!;
    if (row["kind"] !== thread.kind) {
      throw new Error(`thread #${thread.number} is kind ${String(row["kind"])}, not ${thread.kind}`);
    }
    return row["id"] as number;
  }

  getThread(threadId: number): Thread {
    const row = this.#get("SELECT * FROM thread WHERE id = ?", threadId);
    if (row === undefined) throw new Error(`no thread ${threadId}`);
    return {
      id: row["id"] as number,
      sourceId: row["source_id"] as number,
      kind: row["kind"] as ThreadKind,
      number: row["number"] as number,
      url: row["url"] as string,
    };
  }

  // --- triggers ------------------------------------------------------------------------------

  /** Records an event once. A repeat of the same source event id is a no-op marked duplicate. */
  insertTrigger(trigger: NewTrigger): Inserted {
    const inserted = this.#get(
      `INSERT INTO "trigger"
         (source_id, source_event_id, thread_id, kind, actor, url, occurred_at, received_at)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT (source_id, source_event_id) DO NOTHING
       RETURNING id`,
      trigger.sourceId,
      trigger.sourceEventId,
      trigger.threadId,
      trigger.kind,
      trigger.actor,
      trigger.url,
      trigger.occurredAt.toISOString(),
      this.#stamp(),
    );
    if (inserted !== undefined) return { id: inserted["id"] as number, duplicate: false };
    const existing = this.#get(
      `SELECT id FROM "trigger" WHERE source_id = ? AND source_event_id = ?`,
      trigger.sourceId,
      trigger.sourceEventId,
    )!;
    return { id: existing["id"] as number, duplicate: true };
  }

  // --- decisions and tickets -----------------------------------------------------------------

  /** Records the Router's judgement. An accept opens a ticket in the same transaction. */
  recordDecision(decision: NewDecision): { decisionId: number; ticketId: number | null } {
    return this.#transaction(() => {
      const at = this.#stamp();
      const decisionId = this.#get(
        "INSERT INTO decision (thread_id, verdict, reason, decided_at) VALUES (?, ?, ?, ?) RETURNING id",
        decision.threadId,
        decision.verdict,
        decision.reason,
        at,
      )!["id"] as number;
      for (const triggerId of decision.triggerIds) {
        const row = this.#get(`SELECT thread_id FROM "trigger" WHERE id = ?`, triggerId);
        if (row === undefined) throw new Error(`no trigger ${triggerId}`);
        if (row["thread_id"] !== decision.threadId) {
          throw new Error(`trigger ${triggerId} is not on thread ${decision.threadId}`);
        }
        this.#run(
          "INSERT INTO decision_trigger (decision_id, trigger_id) VALUES (?, ?)",
          decisionId,
          triggerId,
        );
      }
      if (decision.verdict !== "accept") return { decisionId, ticketId: null };
      const ticketId = this.#get(
        "INSERT INTO ticket (decision_id, state, opened_at) VALUES (?, 'open', ?) RETURNING id",
        decisionId,
        at,
      )!["id"] as number;
      return { decisionId, ticketId };
    });
  }

  getTicket(ticketId: number): Ticket {
    const row = this.#get("SELECT * FROM ticket WHERE id = ?", ticketId);
    if (row === undefined) throw new Error(`no ticket ${ticketId}`);
    return toTicket(row);
  }

  endTicket(ticketId: number): void {
    const { changes } = this.#run(
      "UPDATE ticket SET state = 'ended', ended_at = ? WHERE id = ? AND state = 'open'",
      this.#stamp(),
      ticketId,
    );
    if (Number(changes) !== 1) {
      throw new IllegalTransition(
        `ticket ${ticketId}: ${this.getTicket(ticketId).state} → ended is not allowed`,
      );
    }
  }

  // --- assignments ---------------------------------------------------------------------------

  /** Binds an open ticket to a Worker that is about to be created. */
  createAssignment(ticketId: number): number {
    return this.#transaction(() => {
      const ticket = this.getTicket(ticketId);
      if (ticket.state !== "open") {
        throw new IllegalTransition(`ticket ${ticketId} is ${ticket.state}; it takes no assignment`);
      }
      return this.#get(
        "INSERT INTO assignment (ticket_id, state, created_at) VALUES (?, 'pending', ?) RETURNING id",
        ticketId,
        this.#stamp(),
      )!["id"] as number;
    });
  }

  getAssignment(assignmentId: number): Assignment {
    const row = this.#get("SELECT * FROM assignment WHERE id = ?", assignmentId);
    if (row === undefined) throw new Error(`no assignment ${assignmentId}`);
    return toAssignment(row);
  }

  /** `pending → running`, recording the T3 Code thread the Worker runs in. */
  startAssignment(assignmentId: number, t3ThreadId: string): void {
    if (t3ThreadId === "") throw new Error("t3ThreadId must not be empty");
    const { changes } = this.#run(
      `UPDATE assignment SET state = 'running', t3_thread_id = ?, started_at = ?
       WHERE id = ? AND state = 'pending'`,
      t3ThreadId,
      this.#stamp(),
      assignmentId,
    );
    if (Number(changes) !== 1) this.#illegalAssignment(assignmentId, "running");
  }

  /** `running → ended`. */
  endAssignment(assignmentId: number): void {
    const { changes } = this.#run(
      "UPDATE assignment SET state = 'ended', ended_at = ? WHERE id = ? AND state = 'running'",
      this.#stamp(),
      assignmentId,
    );
    if (Number(changes) !== 1) this.#illegalAssignment(assignmentId, "ended");
  }

  #illegalAssignment(assignmentId: number, to: AssignmentState): never {
    const from = this.getAssignment(assignmentId).state;
    throw new IllegalTransition(`assignment ${assignmentId}: ${from} → ${to} is not allowed`);
  }

  // --- subscriptions -------------------------------------------------------------------------

  /** Subscribes a ticket to a thread. Subscribing twice is a no-op marked duplicate. */
  subscribe(ticketId: number, threadId: number): { duplicate: boolean } {
    const { changes } = this.#run(
      "INSERT INTO subscription (ticket_id, thread_id, subscribed_at) VALUES (?, ?, ?) ON CONFLICT DO NOTHING",
      ticketId,
      threadId,
      this.#stamp(),
    );
    return { duplicate: Number(changes) === 0 };
  }

  /** Every ticket subscribed to the thread, open or ended, oldest first. */
  ticketsForThread(threadId: number): Ticket[] {
    return this.#all(
      `SELECT ticket.* FROM subscription JOIN ticket ON ticket.id = subscription.ticket_id
       WHERE subscription.thread_id = ? ORDER BY ticket.id`,
      threadId,
    ).map(toTicket);
  }

  // --- outbox --------------------------------------------------------------------------------

  /**
   * Queues a reply under a stable id, due now. Enqueueing the same id and content again is a
   * no-op marked duplicate; the same id with different content is an error.
   */
  enqueueReply(reply: NewReply): { duplicate: boolean } {
    return this.#transaction(() => {
      const existing = this.#get("SELECT * FROM outbox WHERE id = ?", reply.id);
      if (existing !== undefined) {
        const queued = toReply(existing);
        if (queued.threadId !== reply.threadId || queued.body !== reply.body) {
          throw new Error(`outbox ${reply.id} already holds a different reply`);
        }
        return { duplicate: true };
      }
      const at = this.#stamp();
      this.#run(
        "INSERT INTO outbox (id, thread_id, body, enqueued_at, next_attempt_at) VALUES (?, ?, ?, ?, ?)",
        reply.id,
        reply.threadId,
        reply.body,
        at,
        at,
      );
      return { duplicate: false };
    });
  }

  /** Unsent replies whose next attempt is due, oldest due first. */
  dueReplies(limit = 50): Reply[] {
    return this.#all(
      `SELECT * FROM outbox WHERE sent_at IS NULL AND next_attempt_at <= ?
       ORDER BY next_attempt_at, enqueued_at, id LIMIT ?`,
      this.#stamp(),
      limit,
    ).map(toReply);
  }

  /** Marks a reply sent. Marking it sent a second time throws. */
  markReplySent(replyId: string): void {
    const { changes } = this.#run(
      "UPDATE outbox SET sent_at = ?, attempts = attempts + 1 WHERE id = ? AND sent_at IS NULL",
      this.#stamp(),
      replyId,
    );
    if (Number(changes) !== 1) this.#unsentReplyMissing(replyId);
  }

  /** Records a failed send and when to try again. A sent reply cannot fail afterwards. */
  recordReplyFailure(replyId: string, error: string, nextAttemptAt: Date): void {
    const { changes } = this.#run(
      `UPDATE outbox SET attempts = attempts + 1, last_error = ?, next_attempt_at = ?
       WHERE id = ? AND sent_at IS NULL`,
      error,
      nextAttemptAt.toISOString(),
      replyId,
    );
    if (Number(changes) !== 1) this.#unsentReplyMissing(replyId);
  }

  #unsentReplyMissing(replyId: string): never {
    const row = this.#get("SELECT sent_at FROM outbox WHERE id = ?", replyId);
    if (row === undefined) throw new Error(`no outbox reply ${replyId}`);
    throw new IllegalTransition(`outbox ${replyId} was already sent at ${String(row["sent_at"])}`);
  }

  // --- plumbing ------------------------------------------------------------------------------

  #stamp(): string {
    return this.#now().toISOString();
  }

  #get(sql: string, ...params: SQLInputValue[]): Row | undefined {
    return this.#db.prepare(sql).get(...params);
  }

  #all(sql: string, ...params: SQLInputValue[]): Row[] {
    return this.#db.prepare(sql).all(...params);
  }

  #run(sql: string, ...params: SQLInputValue[]) {
    return this.#db.prepare(sql).run(...params);
  }

  #transaction<T>(body: () => T): T {
    this.#db.exec("BEGIN IMMEDIATE");
    try {
      const result = body();
      this.#db.exec("COMMIT");
      return result;
    } catch (error) {
      this.#db.exec("ROLLBACK");
      throw error;
    }
  }
}

// STRICT tables guarantee the column types, so these casts restate the schema, not guess at it.

function toSource(row: Row): Source {
  return {
    id: row["id"] as number,
    kind: row["kind"] as SourceKind,
    name: row["name"] as string,
    cursor: row["cursor"] as string | null,
  };
}

function toTicket(row: Row): Ticket {
  return {
    id: row["id"] as number,
    decisionId: row["decision_id"] as number,
    state: row["state"] as TicketState,
    openedAt: row["opened_at"] as string,
    endedAt: row["ended_at"] as string | null,
  };
}

function toAssignment(row: Row): Assignment {
  return {
    id: row["id"] as number,
    ticketId: row["ticket_id"] as number,
    state: row["state"] as AssignmentState,
    t3ThreadId: row["t3_thread_id"] as string | null,
    createdAt: row["created_at"] as string,
    startedAt: row["started_at"] as string | null,
    endedAt: row["ended_at"] as string | null,
  };
}

function toReply(row: Row): Reply {
  return {
    id: row["id"] as string,
    threadId: row["thread_id"] as number,
    body: row["body"] as string,
    enqueuedAt: row["enqueued_at"] as string,
    attempts: row["attempts"] as number,
    nextAttemptAt: row["next_attempt_at"] as string,
    lastError: row["last_error"] as string | null,
    sentAt: row["sent_at"] as string | null,
  };
}
