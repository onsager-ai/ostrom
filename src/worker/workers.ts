// Native Workers (docs/direction.md, milestone 1 PR 2): each Worker is a Claude Code or Codex
// session started and resumed by ID through the session runner. Turns on one session run one at a
// time; a follow-up queues behind the running turn. Everything here is deterministic.

import { basename, join } from "node:path";

import { turnBudgetReached } from "../budget/budgets.ts";
import { queueStatusCard } from "../github/cards.ts";
import { ghGet, type GhRunner } from "../github/gh.ts";
import { renderReviewReply } from "../github/review-reply.ts";
import { parseStuckReply, renderStuckReply } from "../watchdog/stuck.ts";
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
  /** GitHub access is needed only to restore stop cleanup after a daemon restart. */
  gh?: GhRunner;
  repo?: string;
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

export function workerTurnPrompt(prompt: string): string {
  const contract = "Do not merge, enable auto-merge, dismiss reviews, or resolve human review threads.";
  return prompt.includes(contract) ? prompt : `${prompt}\n\n${contract}`;
}

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
  /** Live callback source only; durable queued_turn rows remain the scheduling authority. */
  readonly #callbacksByKey = new Map<string, {
    onStart: ((turnId: string) => void) | undefined;
    onComplete: ((result: TurnResult) => void | Promise<void>) | undefined;
  }>();
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
      assignmentId, deliveryKey: key, kind, prompt: workerTurnPrompt(prompt), ...(sourcePointer === undefined ? {} : { sourcePointer }),
    });
    if (added.duplicate) return false;
    this.#callbacksByKey.set(this.#callbackKey(assignmentId, key), { onStart, onComplete });
    this.#schedule(assignmentId, key);
    return true;
  }

  /** Releases a merge reservation and wakes every queued turn on that assignment. */
  release(assignmentId: number): void {
    this.#deps.store.releaseAssignmentReservation(assignmentId);
    this.#scheduleQueued(assignmentId);
  }

  /** Wakes queued work after an external state change such as resume. */
  wake(assignmentId: number): void {
    this.#scheduleQueued(assignmentId);
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
        }, "recovery", turnId);
      }
      this.#scheduleQueued(assignment.id);
    }
    return pass;
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

  #callbackKey(id: number, key: string): string {
    return `${id}:${key}`;
  }

  #scheduleQueued(id: number): void {
    for (const turn of this.#deps.store.unfinishedTurns(id)) {
      if (turn.state === "queued") this.#schedule(id, turn.deliveryKey);
    }
  }

  #schedule(id: number, key: string): void {
    const callbacks = this.#callbacksByKey.get(this.#callbackKey(id, key)) ?? this.#callbacks(id, key);
    void this.#enqueue(id, () => this.#turn(id, key, callbacks.onStart, callbacks.onComplete));
  }

  /** Rebuilds the deterministic side effects for a persisted turn after a daemon restart. */
  #callbacks(id: number, key: string): { onStart?: (turnId: string) => void; onComplete?: (result: TurnResult) => void | Promise<void> } {
    const turn = this.#deps.store.queuedTurn(id, key);
    if (turn === null) return {};
    const store = this.#deps.store;
    if (turn.kind === "follow_up" && turn.sourcePointer !== null) {
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
    if (turn.kind === "review" && turn.sourcePointer !== null) {
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
    if (turn.kind === "control") return this.#restoredControl(id, key);
    if (turn.kind === "budget_pause") return { onComplete: () => queueStatusCard(store, store.getAssignment(id).ticketId) };
    if (turn.kind === "stuck") return this.#restoredStuck(id, key);
    if (turn.kind === "recovery" && turn.sourcePointer !== null) {
      return {
        onComplete: (result) => {
          for (const review of store.reviewDeliveries(id, turn.sourcePointer!)) {
            store.enqueueReply({ id: `review-${id}-${review.reviewId}`, threadId: review.threadId, body: renderReviewReply(result.text) });
          }
        },
      };
    }
    return {};
  }

  #restoredControl(id: number, key: string): { onComplete?: (result: TurnResult) => Promise<void> } {
    if (!/^(operator-stop-|stop-|close-issue-)/.test(key)) return {};
    const { store, gh, repo } = this.#deps;
    return {
      onComplete: async (result) => {
        const assignment = store.getAssignment(id);
        const issueClosed = key.startsWith("close-issue-");
        if (gh !== undefined && repo !== undefined && assignment.branch !== null) {
          for (const pr of await openPullRequests(gh, repo, assignment.branch)) await gh(["pr", "ready", "--undo", pr]);
        }
        store.endAssignment(id, issueClosed ? "issue closed" : "stopped by command");
        store.endTicket(assignment.ticketId);
        const replyId = issueClosed ? `stopped-issue-closed-${assignment.ticketId}` :
          key.startsWith("operator-stop-") ? `operator-stopped-${assignment.ticketId}` : `stopped-${assignment.ticketId}`;
        store.enqueueReply({
          id: replyId,
          threadId: store.ticketContext(assignment.ticketId).thread.id,
          body: result.text,
          blocking: true,
        });
        queueStatusCard(store, assignment.ticketId);
      },
    };
  }

  #restoredStuck(id: number, key: string): { onComplete?: (result: TurnResult) => void } {
    if (!key.startsWith("stuck-public-")) return {};
    const stallId = Number(key.slice("stuck-public-".length));
    const store = this.#deps.store;
    const assignment = store.getAssignment(id);
    return {
      onComplete: (turn) => {
        const stall = store.getStall(stallId);
        const cause = stall.cause ?? stall.semonReason ?? "unknown";
        const reply = renderStuckReply(parseStuckReply(turn.text, cause));
        store.enqueueReply({ id: `stuck-${stall.id}`, threadId: store.ticketContext(assignment.ticketId).thread.id, body: reply, blocking: true });
        queueStatusCard(store, assignment.ticketId);
      },
    };
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
    // No await separates this check from starting the native turn, so a held reservation never
    // increments the ticket's turn budget.
    if (store.reservationHeld(id)) return;
    if (store.queuedTurn(id, key)?.state !== "queued") return;
    const session = this.#session(assignment);
    const nextTurn = assignment.turns + 1;
    const turnId = `${assignment.sessionId}:${nextTurn}`;
    if (store.beginQueuedTurn(id, key, turnId) === null) return;
    const turn = store.beginTurn(id);
    if (turn !== nextTurn) throw new Error(`assignment ${id}: queued turn number changed`);
    onStart?.(turnId);
    const controller = new AbortController();
    this.#inFlight.set(id, controller);
    let result: TurnResult;
    try {
      result = await session.send(workerTurnPrompt(queued.prompt), controller.signal);
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
    this.#callbacksByKey.delete(this.#callbackKey(id, key));
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

async function openPullRequests(gh: GhRunner, repo: string, branch: string): Promise<string[]> {
  const owner = repo.split("/")[0]!;
  const response = await ghGet(gh, `repos/${repo}/pulls?head=${encodeURIComponent(`${owner}:${branch}`)}&state=open&per_page=100`);
  if (!Array.isArray(response)) throw new Error("expected open pull requests");
  return response.map((value) => {
    const url = (value as { html_url?: unknown }).html_url;
    if (typeof url !== "string") throw new Error("open pull request has no URL");
    return url;
  });
}
