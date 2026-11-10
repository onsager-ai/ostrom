// Native Workers (docs/direction.md, milestone 1 PR 2): each Worker is a Claude Code or Codex
// session started and resumed by ID through the session runner. Turns on one session run one at a
// time; a follow-up queues behind the running turn. Everything here is deterministic.

import { basename, join } from "node:path";

import { turnBudgetReached } from "../budget/budgets.ts";
import { queueStatusCard } from "../github/cards.ts";
import { renderReviewReply } from "../github/review-reply.ts";
import type { WorkerConfig } from "../ostromd/config.ts";
import { rolloutModel } from "../session/rollout.ts";
import type { Runner, Session, TurnResult } from "../session/runner.ts";
import { createWorktree } from "../session/worktree.ts";
import type { Assignment, AssignmentOutcome, QueuedTurnKind, Store } from "../store/store.ts";
import { workerBrief } from "./dispatch.ts";

export interface Dispatched {
  assignmentId: number;
  sessionId: string;
  branch: string;
}

export interface WorkersDeps {
  store: Store;
  config: WorkerConfig;
  runners: Record<"claude" | "codex", Runner>;
  /** Where Codex writes rollout records; set to verify the Worker's model after each Codex turn. */
  codexSessions?: string;
  addWorktree?: typeof createWorktree;
  log?: (line: string) => void;
}

/** The writable Git metadata for a linked worktree's ordinary commits. */
export function gitDirs(repo: string, worktree: string): string[] {
  const git = join(repo, ".git");
  return [
    join(git, "objects"),
    join(git, "refs"),
    join(git, "logs"),
    join(git, "worktrees", basename(worktree)),
  ];
}

/** Turns that run even on a paused ticket: those that pause it, and those that stop it. */
const RUNS_WHILE_PAUSED = ["budget-pause-", "stuck-", "operator-pause-", "operator-stop-", "pause-", "stop-", "close-issue-"];

export const INTERRUPTED_PROMPT =
  "You were interrupted while working on this ticket. Check your state (git status, your branch and " +
  "pull request) and continue where you stopped.";

function queuedTurnKind(key: string): QueuedTurnKind {
  if (key.startsWith("review-")) return "review";
  if (key.startsWith("budget-pause-")) return "budget_pause";
  if (key.startsWith("stuck-")) return "stuck";
  if (key.startsWith("recovery-")) return "recovery";
  if (/^(operator-|pause-|resume-|stop-|close-issue-)/.test(key)) return "control";
  return "follow_up";
}

export class Workers {
  readonly #deps: WorkersDeps;
  readonly #sessions = new Map<number, Session>();
  readonly #chains = new Map<number, Promise<void>>();
  readonly #budgetPauses = new Set<number>();
  readonly #inFlight = new Map<number, AbortController>();
  #stopping = false;

  constructor(deps: WorkersDeps) {
    this.#deps = deps;
  }

  /**
   * Creates the ticket's worktree and assignment, starts the session with the Worker brief and
   * returns once its ID is known and stored; the turn goes on in the background (see `drain`).
   * A session that does not start ends the assignment with the error as its reason and rethrows.
   */
  async dispatch(ticketId: number): Promise<Dispatched> {
    const { store, config } = this.#deps;
    const context = store.ticketContext(ticketId);
    const id = store.createAssignment(ticketId);
    try {
      const path = join(config.worktrees, `ticket-${ticketId}-${id}`);
      const branch = await (this.#deps.addWorktree ?? createWorktree)(config.repo, path, config.base, ticketId, id);
      store.configureNativeAssignment(id, { harness: config.harness, model: config.model, worktree: path, branch, machine: config.machine });
      let resolve!: (sessionId: string) => void;
      const started = new Promise<string>((r) => (resolve = r));
      const session = this.#deps.runners[config.harness].start({
        cwd: path,
        model: config.model,
        writableDirs: gitDirs(config.repo, path),
        onSessionId: (sessionId) => {
          store.startNativeAssignment(id, sessionId);
          store.beginTurn(id);
          queueStatusCard(store, ticketId);
          this.#sessions.set(id, session);
          resolve(sessionId);
        },
      });
      const turn = this.#enqueue(id, () => this.#firstTurn(id, session, workerBrief(context, branch, config.base, config.rules)));
      await Promise.race([started, turn]);
      const assignment = store.getAssignment(id);
      if (assignment.state === "ended") throw new Error(assignment.endReason ?? "the session did not start");
      return { assignmentId: id, sessionId: assignment.sessionId!, branch };
    } catch (error) {
      if (store.getAssignment(id).state === "pending") {
        store.abandonAssignment(id, error instanceof Error ? error.message : String(error));
      }
      throw error;
    }
  }

  /**
   * Persists a follow-up before scheduling it. Delivery acknowledgements remain at turn start, so
   * a crash between receipt and delivery leaves an unfinished row rather than falsely looking idle.
   */
  deliver(
    assignmentId: number,
    key: string,
    prompt: string,
    onStart: (turnId: string) => void,
    onComplete?: (result: TurnResult) => void | Promise<void>,
    kind: QueuedTurnKind = queuedTurnKind(key),
    sourcePointer?: string,
  ): boolean {
    const added = this.#deps.store.enqueueTurn({
      assignmentId, deliveryKey: key, kind, prompt, ...(sourcePointer === undefined ? {} : { sourcePointer }),
    });
    if (added.duplicate) return false;
    this.#schedule(assignmentId, key, onStart, onComplete);
    return true;
  }

  /** Cancels the turn currently running for an assignment, if any. */
  cancel(assignmentId: number): boolean {
    const controller = this.#inFlight.get(assignmentId);
    if (controller === undefined || controller.signal.aborted) return false;
    controller.abort();
    return true;
  }

  /**
   * For an ostromd shutdown: cuts off every running turn and starts no queued one. A cut-off turn
   * stays open in the store, so the next start resumes it by session ID (`recover`), as after a crash.
   */
  stop(): void {
    this.#stopping = true;
    for (const controller of this.#inFlight.values()) controller.abort();
  }

  /**
   * Run once at start. A native assignment that never got a session ends; the others resume by
   * session ID, and one whose turn was cut off is sent a turn to check its state and go on.
   */
  recover(): { resumed: number[]; interrupted: number[]; abandoned: number[] } {
    const { store } = this.#deps;
    const pass = { resumed: [] as number[], interrupted: [] as number[], abandoned: [] as number[] };
    for (const pending of store.pendingAssignments()) {
      store.abandonAssignment(pending.id, "ostromd stopped before the session started");
      pass.abandoned.push(pending.id);
    }
    for (const assignment of store.runningAssignments()) {
      if (assignment.sessionId === null) continue;
      this.#session(assignment);
      pass.resumed.push(assignment.id);
      if (assignment.turnStartedAt !== null) {
        const turnId = `${assignment.sessionId}:${assignment.turns}`;
        const reviews = store.reviewDeliveries(assignment.id, turnId);
        store.recordInterruption(assignment.id);
        store.finishRunningTurns(assignment.id);
        store.endTurn(assignment.id, null);
        pass.interrupted.push(assignment.id);
        this.deliver(assignment.id, `recovery-${assignment.interruptions + 1}`, INTERRUPTED_PROMPT, () => {}, (result) => {
          for (const review of reviews) {
            store.enqueueReply({
              id: `review-${assignment.id}-${review.reviewId}`,
              threadId: review.threadId,
              body: renderReviewReply(result.text),
            });
          }
        }, "recovery");
      }
      for (const turn of store.unfinishedTurns(assignment.id)) this.#schedule(assignment.id, turn.deliveryKey);
    }
    this.#recoverStopCleanups();
    return pass;
  }

  /** Finishes a durable stop cleanup if its original callback was lost on process restart. */
  #recoverStopCleanups(): void {
    const { store } = this.#deps;
    for (const cleanup of store.completedStopCleanups()) {
      store.endAssignment(cleanup.assignmentId, cleanup.issueClosed ? "issue closed" : "stopped by command");
      store.endTicket(cleanup.ticketId);
      store.enqueueReply({
        id: cleanup.issueClosed ? `stopped-issue-closed-${cleanup.ticketId}` : `stopped-${cleanup.ticketId}`,
        threadId: cleanup.threadId,
        body: "The ticket was stopped after Ostrom restarted.",
        blocking: true,
      });
      queueStatusCard(store, cleanup.ticketId);
    }
  }

  /** Resolves when every queued and running turn has finished. */
  async drain(): Promise<void> {
    while (this.#chains.size > 0) await Promise.all([...this.#chains.values()]);
  }

  #session(assignment: Assignment): Session {
    let session = this.#sessions.get(assignment.id);
    if (session === undefined) {
      session = this.#deps.runners[assignment.harness!].resume({
        cwd: assignment.worktree!,
        model: assignment.model!,
        sessionId: assignment.sessionId!,
        writableDirs: gitDirs(this.#deps.config.repo, assignment.worktree!),
      });
      this.#sessions.set(assignment.id, session);
    }
    return session;
  }

  #schedule(
    id: number,
    key: string,
    onStart?: (turnId: string) => void,
    onComplete?: (result: TurnResult) => void | Promise<void>,
  ): void {
    const restored = onStart === undefined ? this.#callbacks(id, key) : { onStart, onComplete };
    void this.#enqueue(id, () => this.#turn(id, key, restored.onStart, restored.onComplete));
  }

  /** Rebuilds the deterministic side effects for a persisted follow-up after a daemon restart. */
  #callbacks(id: number, key: string): { onStart?: (turnId: string) => void; onComplete?: (result: TurnResult) => void } {
    const turn = this.#deps.store.queuedTurn(id, key);
    if (turn === null || turn.sourcePointer === null) return {};
    const store = this.#deps.store;
    if (turn.kind === "follow_up") {
      const trigger = store.getTrigger(Number(turn.sourcePointer));
      return {
        onStart: (turnId) => {
          store.recordDelivery(trigger.id, id, turnId);
          if (trigger.kind !== "pull_request_review") {
            store.enqueueReply({ id: `received-${trigger.id}`, threadId: trigger.threadId, body: "", kind: "reaction", target: trigger.url, reaction: "eyes" });
          }
        },
      };
    }
    if (turn.kind === "review") {
      const triggers = turn.sourcePointer.split(",").map((value) => store.getTrigger(Number(value)));
      const review = triggers.find((trigger) => trigger.reviewId !== undefined)!;
      return {
        onStart: (turnId) => {
          store.recordReviewDelivery(triggers.map((trigger) => trigger.id), id, turnId);
          for (const trigger of triggers) {
            if (trigger.kind !== "pull_request_review") {
              store.enqueueReply({ id: `received-${trigger.id}`, threadId: trigger.threadId, body: "", kind: "reaction", target: trigger.url, reaction: "eyes" });
            }
          }
        },
        onComplete: (result) => {
          store.enqueueReply({ id: `review-${id}-${review.reviewId}`, threadId: review.threadId, body: renderReviewReply(result.text) });
        },
      };
    }
    return {};
  }

  #enqueue(id: number, job: () => Promise<void>): Promise<void> {
    const next = (this.#chains.get(id) ?? Promise.resolve()).then(job);
    this.#chains.set(id, next);
    void next.finally(() => {
      if (this.#chains.get(id) === next) this.#chains.delete(id);
    });
    return next;
  }

  async #firstTurn(id: number, session: Session, prompt: string): Promise<void> {
    const { store } = this.#deps;
    const controller = new AbortController();
    this.#inFlight.set(id, controller);
    let result: TurnResult;
    try {
      result = await session.send(prompt, controller.signal);
    } catch (error) {
      if (this.#stopping) return;
      if (controller.signal.aborted) {
        store.endTurn(id, null);
        store.clearTurnError(id);
      } else if (store.getAssignment(id).state === "pending") {
        store.abandonAssignment(id, error instanceof Error ? error.message : String(error));
      } else {
        this.#failed(id, error);
      }
      return;
    } finally {
      if (this.#inFlight.get(id) === controller) this.#inFlight.delete(id);
    }
    if (this.#stopping) return;
    this.#finished(id, result);
  }

  async #turn(
    id: number,
    key: string,
    onStart?: (turnId: string) => void,
    onComplete?: (result: TurnResult) => void | Promise<void>,
  ): Promise<void> {
    const { store } = this.#deps;
    const assignment = store.getAssignment(id);
    const queued = store.queuedTurn(id, key);
    if (queued === null || queued.state !== "queued" || this.#stopping || assignment.state !== "running") return;
    const ticket = store.getTicket(assignment.ticketId);
    if (ticket.state === "paused" && !RUNS_WHILE_PAUSED.some((prefix) => key.startsWith(prefix))) return;
    const session = this.#session(assignment);
    const turn = store.beginTurn(id);
    const turnId = `${assignment.sessionId}:${turn}`;
    if (store.beginQueuedTurn(id, key, turnId) === null) {
      store.endTurn(id, null);
      return;
    }
    onStart?.(turnId);
    const controller = new AbortController();
    this.#inFlight.set(id, controller);
    let result: TurnResult;
    try {
      result = await session.send(queued.prompt, controller.signal);
    } catch (error) {
      if (this.#stopping) return;
      store.finishQueuedTurn(id, key);
      if (controller.signal.aborted) {
        store.endTurn(id, null);
        store.clearTurnError(id);
      } else {
        this.#failed(id, error);
      }
      return;
    } finally {
      if (this.#inFlight.get(id) === controller) this.#inFlight.delete(id);
    }
    if (this.#stopping) return;
    store.finishQueuedTurn(id, key);
    this.#finished(id, result);
    if (result.status === "completed") await onComplete?.(result);
    for (const next of store.unfinishedTurns(id)) {
      if (next.deliveryKey !== key && next.state === "queued") this.#schedule(id, next.deliveryKey);
    }
  }

  #failed(id: number, error: unknown): void {
    const message = error instanceof Error ? error.message : String(error);
    this.#deps.log?.(`assignment ${id}: turn failed: ${message}`);
    this.#deps.store.endTurn(id, null);
    this.#deps.store.recordTurnError(id, message);
    this.#mark(id, "failed");
    queueStatusCard(this.#deps.store, this.#deps.store.getAssignment(id).ticketId);
  }

  #finished(id: number, result: TurnResult): void {
    const { store } = this.#deps;
    store.endTurn(id, result.usage);
    store.clearTurnError(id);
    const assignment = store.getAssignment(id);
    if (assignment.state !== "running") return;
    if (result.status === "cancelled") {
      queueStatusCard(store, assignment.ticketId);
      return;
    }
    const reached = turnBudgetReached(store.ticketBudget(assignment.ticketId), new Date());
    if (store.getTicket(assignment.ticketId).state === "open" && reached !== null && !this.#budgetPauses.has(id)) {
      this.#budgetPauses.add(id);
      store.pauseTicketForBudget(assignment.ticketId, reached);
      this.deliver(
        id,
        `budget-pause-${assignment.ticketId}-${reached}`,
        "The ticket budget has been reached. Push your work in progress, then stop and wait for Ostrom to resume you.",
        () => {},
      );
      queueStatusCard(store, assignment.ticketId);
      return;
    }
    if (store.getTicket(assignment.ticketId).state === "open") this.#budgetPauses.delete(id);
    if (assignment.harness === "codex" && this.#deps.codexSessions !== undefined) {
      const reason = this.#modelProblem(assignment, this.#deps.codexSessions);
      if (reason !== null) {
        this.#deps.log?.(`assignment ${id}: ended: ${reason}`);
        store.endAssignment(id, reason);
        return;
      }
    }
    // Ostrom only aborts a turn to replace it. That cancellation is bookkeeping, not a terminal
    // Worker outcome: the next queued turn must still run on this assignment.
    this.#mark(id, null);
    queueStatusCard(store, assignment.ticketId);
  }

  /** Records an outcome unless the Router ended the assignment while the turn ran. */
  #mark(id: number, outcome: AssignmentOutcome | null): void {
    if (this.#deps.store.getAssignment(id).state === "running") this.#deps.store.recordOutcome(id, outcome);
  }

  #modelProblem(assignment: Assignment, root: string): string | null {
    let model: string;
    try {
      model = rolloutModel(root, assignment.sessionId!);
    } catch (error) {
      return `model unverified: ${error instanceof Error ? error.message : String(error)}`;
    }
    return model === assignment.model ? null : `model mismatch: asked for ${assignment.model}, ran ${model}`;
  }
}
