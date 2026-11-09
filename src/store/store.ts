// Ostrom's one SQLite store: typed access to its own domain records, and nothing else.
// See docs/direction.md, "Data model". Only the operations milestone 1 needs live here.

import { DatabaseSync, type SQLInputValue } from "node:sqlite";
import { migrate } from "./migrations.ts";

export type SourceKind = "github";
export type ThreadKind = "issue" | "pull_request";
export type Verdict = "accept" | "ignore" | "stop";
export type TicketState = "open" | "ended";
export type AssignmentState = "pending" | "running" | "ended";
/** What the last observation of a running Worker's T3 thread found worth flagging. */
export type AssignmentOutcome = "waiting" | "failed" | "cancelled";

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

/** A stored trigger. Timestamps are ISO strings as stored. */
export interface Trigger {
  id: number;
  sourceId: number;
  sourceEventId: string;
  threadId: number;
  kind: string;
  actor: string;
  url: string;
  occurredAt: string;
  receivedAt: string;
}

/**
 * A follow-up owed to a Worker: an undecided trigger on a thread that an open ticket is
 * subscribed to, not yet delivered to that ticket. The assignment is the ticket's latest running
 * one, or null when the ticket has no running Worker to take it.
 */
export interface PendingDelivery {
  trigger: Trigger;
  ticketId: number;
  assignmentId: number | null;
  t3ThreadId: string | null;
}

export interface NewDecision {
  threadId: number;
  triggerIds: readonly number[];
  verdict: Verdict;
  reason: string;
  /** A reply to post on the thread, queued in the same transaction under `decisionReplyId`. */
  reply?: string;
}

/** The outbox id of the reply a decision carried: stable, so the reply is sent once. */
export function decisionReplyId(decisionId: number): string {
  return `decision-${decisionId}`;
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
  harness: "claude" | "codex" | null;
  model: string | null;
  sessionId: string | null;
  worktree: string | null;
  branch: string | null;
  machine: string | null;
  turns: number;
  inputTokens: number;
  cachedInputTokens: number;
  outputTokens: number;
  /** Set while a turn runs; still set when ostromd starts means the turn was interrupted. */
  turnStartedAt: string | null;
  interruptions: number;
  createdAt: string;
  startedAt: string | null;
  endedAt: string | null;
  /** Why the assignment ended; always set when it ended without starting. */
  endReason: string | null;
  outcome: AssignmentOutcome | null;
  outcomeAt: string | null;
}

/** What a Worker brief is built from: the accepted thread, the decision, and its triggers. */
export interface TicketContext {
  ticket: Ticket;
  thread: Thread;
  source: Source;
  reason: string;
  triggerUrls: string[];
}

/** One stall episode of a running assignment (migration 5). */
export interface Stall {
  id: number;
  assignmentId: number;
  semonReason: string | null;
  t3Reason: string | null;
  detail: string | null;
  detectedAt: string;
  clearedAt: string | null;
  action: StallAction | null;
  answeredAt: string | null;
}

export interface NewStall {
  assignmentId: number;
  semonReason: string | null;
  t3Reason: string | null;
  detail: string | null;
}

/** The Router's answer to a flagged stall: leave the Worker be, or stop the work. */
export type StallAction = "wait" | "stop";

/** The outbox id of the reply the Router wrote for a stall: stable, so it is sent once. */
export function stallReplyId(stallId: number): string {
  return `stall-${stallId}`;
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

  /**
   * Triggers that still need the Router, oldest first: no decision has judged them, no Worker has
   * been sent them, and no open ticket is subscribed to their thread (those go to its Worker). A
   * trigger stored before a crash and never routed is still here on the next pass.
   */
  undecidedTriggers(): Trigger[] {
    return this.#all(
      `SELECT * FROM "trigger" AS t
       WHERE NOT EXISTS (SELECT 1 FROM decision_trigger WHERE trigger_id = t.id)
         AND NOT EXISTS (SELECT 1 FROM delivery WHERE trigger_id = t.id)
         AND NOT EXISTS (
           SELECT 1 FROM subscription JOIN ticket ON ticket.id = subscription.ticket_id
           WHERE subscription.thread_id = t.thread_id AND ticket.state = 'open')
       ORDER BY t.occurred_at, t.id`,
    ).map(toTrigger);
  }

  // --- decisions and tickets -----------------------------------------------------------------

  /**
   * Records the Router's judgement. In the same transaction a reply is queued, so a decision never
   * exists without the acknowledgement it carried, and an accept opens a ticket subscribed to the
   * decision's own thread, so later comments there go to the ticket's Worker as follow-ups
   * instead of reaching the Router as new work. The accepted triggers have this decision, so
   * they are never delivered as follow-ups themselves.
   */
  recordDecision(decision: NewDecision): {
    decisionId: number;
    ticketId: number | null;
    replyId: string | null;
  } {
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
      let replyId: string | null = null;
      if (decision.reply !== undefined) {
        if (decision.reply.trim() === "") throw new Error("a decision's reply must not be empty");
        replyId = decisionReplyId(decisionId);
        this.#insertReply({ id: replyId, threadId: decision.threadId, body: decision.reply });
      }
      if (decision.verdict !== "accept") return { decisionId, ticketId: null, replyId };
      const ticketId = this.#get(
        "INSERT INTO ticket (decision_id, state, opened_at) VALUES (?, 'open', ?) RETURNING id",
        decisionId,
        at,
      )!["id"] as number;
      this.#run(
        "INSERT INTO subscription (ticket_id, thread_id, subscribed_at) VALUES (?, ?, ?)",
        ticketId,
        decision.threadId,
        at,
      );
      return { decisionId, ticketId, replyId };
    });
  }

  /** Open tickets that have never had an assignment: accepted, but no Worker was created yet. */
  undispatchedTickets(): number[] {
    return this.#all(
      `SELECT id FROM ticket WHERE state = 'open'
       AND NOT EXISTS (SELECT 1 FROM assignment WHERE assignment.ticket_id = ticket.id)
       ORDER BY id`,
    ).map((row) => row["id"] as number);
  }

  getTicket(ticketId: number): Ticket {
    const row = this.#get("SELECT * FROM ticket WHERE id = ?", ticketId);
    if (row === undefined) throw new Error(`no ticket ${ticketId}`);
    return toTicket(row);
  }

  /** The ticket with the thread, decision reason and trigger URLs that opened it. */
  ticketContext(ticketId: number): TicketContext {
    const ticket = this.getTicket(ticketId);
    const decision = this.#get("SELECT thread_id, reason FROM decision WHERE id = ?", ticket.decisionId)!;
    const thread = this.getThread(decision["thread_id"] as number);
    const source = toSource(this.#get("SELECT * FROM source WHERE id = ?", thread.sourceId)!);
    const triggerUrls = this.#all(
      `SELECT "trigger".url FROM decision_trigger JOIN "trigger" ON "trigger".id = decision_trigger.trigger_id
       WHERE decision_trigger.decision_id = ? ORDER BY "trigger".occurred_at, "trigger".id`,
      ticket.decisionId,
    ).map((row) => row["url"] as string);
    return { ticket, thread, source, reason: decision["reason"] as string, triggerUrls };
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

  configureNativeAssignment(assignmentId: number, options: { harness: "claude" | "codex"; model: string; worktree: string; branch: string; machine: string }): void {
    const { changes } = this.#run(
      `UPDATE assignment SET harness = ?, model = ?, worktree = ?, branch = ?, machine = ? WHERE id = ? AND state = 'pending'`,
      options.harness, options.model, options.worktree, options.branch, options.machine, assignmentId,
    );
    if (Number(changes) !== 1) this.#illegalAssignment(assignmentId, "pending");
  }

  startNativeAssignment(assignmentId: number, sessionId: string): void {
    if (!sessionId) throw new Error("sessionId must not be empty");
    const { changes } = this.#run(
      `UPDATE assignment SET state = 'running', session_id = ?, started_at = ? WHERE id = ? AND state = 'pending'`,
      sessionId, this.#stamp(), assignmentId,
    );
    if (Number(changes) !== 1) this.#illegalAssignment(assignmentId, "running");
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

  /** `running → ended`, with the reason when the Worker was stopped rather than finished. */
  endAssignment(assignmentId: number, reason: string | null = null): void {
    const { changes } = this.#run(
      "UPDATE assignment SET state = 'ended', ended_at = ?, end_reason = ? WHERE id = ? AND state = 'running'",
      this.#stamp(),
      reason,
      assignmentId,
    );
    if (Number(changes) !== 1) this.#illegalAssignment(assignmentId, "ended");
  }

  /** `pending → ended`: the Worker never started, for the reason given. */
  abandonAssignment(assignmentId: number, reason: string): void {
    if (reason === "") throw new Error("reason must not be empty");
    const { changes } = this.#run(
      "UPDATE assignment SET state = 'ended', ended_at = ?, end_reason = ? WHERE id = ? AND state = 'pending'",
      this.#stamp(),
      reason,
      assignmentId,
    );
    if (Number(changes) !== 1) this.#illegalAssignment(assignmentId, "ended");
  }

  /** Assignments that ended with a reason (never started, or stopped), on open tickets, oldest first. */
  abandonedAssignments(): Assignment[] {
    return this.#all(
      `SELECT assignment.* FROM assignment JOIN ticket ON ticket.id = assignment.ticket_id
       WHERE assignment.state = 'ended' AND assignment.t3_thread_id IS NULL
         AND assignment.end_reason IS NOT NULL AND ticket.state = 'open'
       ORDER BY assignment.id`,
    ).map(toAssignment);
  }

  /** Assignments created but not yet started, oldest first. */
  pendingAssignments(): Assignment[] {
    return this.#all("SELECT * FROM assignment WHERE state = 'pending' ORDER BY id").map(toAssignment);
  }

  /** Marks a native turn as running and returns the assignment's turn number. */
  beginTurn(assignmentId: number): number {
    const row = this.#get(
      "UPDATE assignment SET turns = turns + 1, turn_started_at = ? WHERE id = ? AND state = 'running' RETURNING turns",
      this.#stamp(),
      assignmentId,
    );
    if (row === undefined) this.#illegalAssignment(assignmentId, "running");
    return row!["turns"] as number;
  }

  /** Clears the running turn and adds its usage, when it reported any. */
  endTurn(assignmentId: number, usage: { inputTokens: number; cachedInputTokens: number; outputTokens: number } | null): void {
    this.#run(
      `UPDATE assignment SET turn_started_at = NULL, input_tokens = input_tokens + ?,
         cached_input_tokens = cached_input_tokens + ?, output_tokens = output_tokens + ? WHERE id = ?`,
      usage?.inputTokens ?? 0,
      usage?.cachedInputTokens ?? 0,
      usage?.outputTokens ?? 0,
      assignmentId,
    );
  }

  /** Counts a turn that a restart found unfinished. */
  recordInterruption(assignmentId: number): void {
    this.#run("UPDATE assignment SET interruptions = interruptions + 1 WHERE id = ?", assignmentId);
  }

  /** Every running assignment, oldest first. */
  runningAssignments(): Assignment[] {
    return this.#all("SELECT * FROM assignment WHERE state = 'running' ORDER BY id").map(toAssignment);
  }

  /**
   * Records what the latest look at a running Worker's thread found, or null for nothing to
   * flag. The timestamp moves only when the outcome changes, so it says since when it holds.
   */
  recordOutcome(assignmentId: number, outcome: AssignmentOutcome | null): void {
    const { changes } = this.#run(
      `UPDATE assignment
       SET outcome_at = CASE WHEN outcome IS ? THEN outcome_at WHEN ? IS NULL THEN NULL ELSE ? END,
           outcome = ?
       WHERE id = ? AND state = 'running'`,
      outcome,
      outcome,
      this.#stamp(),
      outcome,
      assignmentId,
    );
    if (Number(changes) !== 1) {
      throw new IllegalTransition(
        `assignment ${assignmentId} is ${this.getAssignment(assignmentId).state}; only a running one has an outcome`,
      );
    }
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

  // --- deliveries ----------------------------------------------------------------------------

  /** Every follow-up owed to an open ticket's Worker, oldest trigger first, then by ticket. */
  pendingDeliveries(): PendingDelivery[] {
    return this.#all(
      `SELECT t.*, ticket.id AS p_ticket_id, worker.id AS p_assignment_id, worker.t3_thread_id AS p_t3_thread_id
       FROM "trigger" AS t
       JOIN subscription ON subscription.thread_id = t.thread_id
       JOIN ticket ON ticket.id = subscription.ticket_id AND ticket.state = 'open'
       LEFT JOIN assignment AS worker ON worker.id = (
         SELECT id FROM assignment WHERE ticket_id = ticket.id AND state = 'running' ORDER BY id DESC LIMIT 1)
       WHERE NOT EXISTS (SELECT 1 FROM decision_trigger WHERE trigger_id = t.id)
         AND NOT EXISTS (
           SELECT 1 FROM delivery JOIN assignment ON assignment.id = delivery.assignment_id
           WHERE delivery.trigger_id = t.id AND assignment.ticket_id = ticket.id)
       ORDER BY t.occurred_at, t.id, ticket.id`,
    ).map((row) => ({
      trigger: toTrigger(row),
      ticketId: row["p_ticket_id"] as number,
      assignmentId: row["p_assignment_id"] as number | null,
      t3ThreadId: row["p_t3_thread_id"] as string | null,
    }));
  }

  /**
   * Records that the trigger reached the assignment's Worker as T3 run `t3RunId`. Recording the
   * same delivery again is a no-op marked duplicate.
   */
  recordDelivery(triggerId: number, assignmentId: number, t3RunId: string): { duplicate: boolean } {
    const { changes } = this.#run(
      `INSERT INTO delivery (trigger_id, assignment_id, t3_run_id, delivered_at) VALUES (?, ?, ?, ?)
       ON CONFLICT DO NOTHING`,
      triggerId,
      assignmentId,
      t3RunId,
      this.#stamp(),
    );
    return { duplicate: Number(changes) === 0 };
  }

  // --- stalls -------------------------------------------------------------------------------

  /**
   * Opens a stall episode for the assignment, unless one is open already: an ongoing stall is
   * recorded once, however many passes see it. Returns the open episode's id.
   */
  recordStall(stall: NewStall): Inserted {
    return this.#transaction(() => {
      const assignment = this.getAssignment(stall.assignmentId);
      if (assignment.state !== "running") {
        throw new IllegalTransition(`assignment ${stall.assignmentId} is ${assignment.state}; only a running one stalls`);
      }
      const open = this.#get("SELECT id FROM stall WHERE assignment_id = ? AND cleared_at IS NULL", stall.assignmentId);
      if (open !== undefined) return { id: open["id"] as number, duplicate: true };
      const id = this.#get(
        `INSERT INTO stall (assignment_id, semon_reason, t3_reason, detail, detected_at)
         VALUES (?, ?, ?, ?, ?) RETURNING id`,
        stall.assignmentId,
        stall.semonReason,
        stall.t3Reason,
        stall.detail,
        this.#stamp(),
      )!["id"] as number;
      return { id, duplicate: false };
    });
  }

  /** Ends the assignment's open stall episode, if any. Returns its id, or null when none was open. */
  clearStall(assignmentId: number): number | null {
    const row = this.#get(
      "UPDATE stall SET cleared_at = ? WHERE assignment_id = ? AND cleared_at IS NULL RETURNING id",
      this.#stamp(),
      assignmentId,
    );
    return row === undefined ? null : (row["id"] as number);
  }

  getStall(stallId: number): Stall {
    const row = this.#get("SELECT * FROM stall WHERE id = ?", stallId);
    if (row === undefined) throw new Error(`no stall ${stallId}`);
    return toStall(row);
  }

  /** Open stall episodes the Router has not answered, on running assignments, oldest first. */
  unansweredStalls(): Stall[] {
    return this.#all(
      `SELECT stall.* FROM stall JOIN assignment ON assignment.id = stall.assignment_id
       WHERE stall.cleared_at IS NULL AND stall.answered_at IS NULL AND assignment.state = 'running'
       ORDER BY stall.id`,
    ).map(toStall);
  }

  /**
   * Records the Router's answer to a stall. In the same transaction its reply, if any, is queued
   * on the thread the ticket was accepted on, and a `stop` ends the assignment and, once no
   * assignment of it runs, the ticket. Code never stops work on a stall without this answer.
   */
  answerStall(
    stallId: number,
    action: StallAction,
    reply: string | null,
  ): { replyId: string | null; ticketId: number; ticketEnded: boolean } {
    return this.#transaction(() => {
      const { changes } = this.#run(
        "UPDATE stall SET action = ?, answered_at = ? WHERE id = ? AND answered_at IS NULL",
        action,
        this.#stamp(),
        stallId,
      );
      if (Number(changes) !== 1) {
        throw new IllegalTransition(`stall ${stallId} was already answered ${String(this.getStall(stallId).action)}`);
      }
      const assignment = this.getAssignment(this.getStall(stallId).assignmentId);
      let replyId: string | null = null;
      if (reply !== null) {
        replyId = stallReplyId(stallId);
        const threadId = this.ticketContext(assignment.ticketId).thread.id;
        this.#insertReply({ id: replyId, threadId, body: reply });
      }
      if (action === "wait") return { replyId, ticketId: assignment.ticketId, ticketEnded: false };
      return { replyId, ticketId: assignment.ticketId, ticketEnded: this.#endWork(assignment) };
    });
  }

  // --- the Router ----------------------------------------------------------------------------

  /** The T3 Code thread the Router currently runs in, or null before the first one starts. */
  routerThread(): string | null {
    const row = this.#get("SELECT t3_thread_id FROM router_thread ORDER BY id DESC LIMIT 1");
    return row === undefined ? null : (row["t3_thread_id"] as string);
  }

  /** Records a newly started Router thread; it becomes the current one. */
  recordRouterThread(t3ThreadId: string): void {
    this.#run(
      "INSERT INTO router_thread (t3_thread_id, started_at) VALUES (?, ?)",
      t3ThreadId,
      this.#stamp(),
    );
  }

  // --- outbox --------------------------------------------------------------------------------

  /**
   * Queues a reply under a stable id, due now. Enqueueing the same id and content again is a
   * no-op marked duplicate; the same id with different content is an error.
   */
  enqueueReply(reply: NewReply): { duplicate: boolean } {
    return this.#transaction(() => this.#insertReply(reply));
  }

  /**
   * Queues an outcome reply. With `end`, the outcome is terminal: in the same transaction the
   * assignment ends if it is still running, and its ticket ends once no assignment of it is
   * running. A crash therefore never leaves a reply queued for work that still looks live, nor
   * work ended without its reply. Returns whether the ticket ended.
   */
  enqueueOutcomeReply(reply: NewReply, end?: { assignmentId: number }): { duplicate: boolean; ticketEnded: boolean } {
    return this.#transaction(() => {
      const { duplicate } = this.#insertReply(reply);
      if (end === undefined) return { duplicate, ticketEnded: false };
      return { duplicate, ticketEnded: this.#endWork(this.getAssignment(end.assignmentId)) };
    });
  }

  /**
   * Ends the assignment if it still runs, and its ticket once no assignment of it runs. Call
   * inside a transaction. Returns whether the ticket ended.
   */
  #endWork(assignment: Assignment): boolean {
    if (assignment.state === "running") this.endAssignment(assignment.id);
    const stillRunning = this.#get(
      "SELECT 1 FROM assignment WHERE ticket_id = ? AND state = 'running'",
      assignment.ticketId,
    );
    if (stillRunning !== undefined || this.getTicket(assignment.ticketId).state !== "open") return false;
    this.endTicket(assignment.ticketId);
    return true;
  }

  /** Whether a reply with this id was ever queued, sent or not. */
  hasReply(replyId: string): boolean {
    return this.#get("SELECT 1 FROM outbox WHERE id = ?", replyId) !== undefined;
  }

  #insertReply(reply: NewReply): { duplicate: boolean } {
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

function toTrigger(row: Row): Trigger {
  return {
    id: row["id"] as number,
    sourceId: row["source_id"] as number,
    sourceEventId: row["source_event_id"] as string,
    threadId: row["thread_id"] as number,
    kind: row["kind"] as string,
    actor: row["actor"] as string,
    url: row["url"] as string,
    occurredAt: row["occurred_at"] as string,
    receivedAt: row["received_at"] as string,
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
    harness: row["harness"] as Assignment["harness"],
    model: row["model"] as string | null,
    sessionId: row["session_id"] as string | null,
    worktree: row["worktree"] as string | null,
    branch: row["branch"] as string | null,
    machine: row["machine"] as string | null,
    turns: row["turns"] as number,
    inputTokens: row["input_tokens"] as number,
    cachedInputTokens: row["cached_input_tokens"] as number,
    outputTokens: row["output_tokens"] as number,
    turnStartedAt: row["turn_started_at"] as string | null,
    interruptions: row["interruptions"] as number,
    createdAt: row["created_at"] as string,
    startedAt: row["started_at"] as string | null,
    endedAt: row["ended_at"] as string | null,
    endReason: row["end_reason"] as string | null,
    outcome: row["outcome"] as AssignmentOutcome | null,
    outcomeAt: row["outcome_at"] as string | null,
  };
}

function toStall(row: Row): Stall {
  return {
    id: row["id"] as number,
    assignmentId: row["assignment_id"] as number,
    semonReason: row["semon_reason"] as string | null,
    t3Reason: row["t3_reason"] as string | null,
    detail: row["detail"] as string | null,
    detectedAt: row["detected_at"] as string,
    clearedAt: row["cleared_at"] as string | null,
    action: row["action"] as StallAction | null,
    answeredAt: row["answered_at"] as string | null,
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
