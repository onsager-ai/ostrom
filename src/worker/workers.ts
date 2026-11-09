// Native Workers (docs/direction.md, milestone 1 PR 2): each Worker is a Claude Code or Codex
// session started and resumed by ID through the session runner. Turns on one session run one at a
// time; a follow-up queues behind the running turn. Everything here is deterministic.

import { join } from "node:path";

import { turnBudgetReached } from "../budget/budgets.ts";
import { queueStatusCard } from "../github/cards.ts";
import type { WorkerConfig } from "../ostromd/config.ts";
import { rolloutModel } from "../session/rollout.ts";
import type { Runner, Session, TurnResult } from "../session/runner.ts";
import { createWorktree } from "../session/worktree.ts";
import type { Assignment, AssignmentOutcome, Store } from "../store/store.ts";
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

/** The main clone's Git directory, where a worktree's commits are written. */
export function gitDir(repo: string): string {
  return join(repo, ".git");
}

export const INTERRUPTED_PROMPT =
  "You were interrupted while working on this ticket. Check your state (git status, your branch and " +
  "pull request) and continue where you stopped.";

export class Workers {
  readonly #deps: WorkersDeps;
  readonly #sessions = new Map<number, Session>();
  readonly #chains = new Map<number, Promise<void>>();
  readonly #queued = new Set<string>();
  readonly #budgetPauses = new Set<number>();

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
        writableDirs: [gitDir(config.repo)],
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
   * Queues a follow-up turn on the assignment's session. `onStart` runs when the turn begins, with
   * the turn's id; that is when the caller records delivery, so a message lost with the queue in a
   * crash is sent again. Returns false when the same key is already queued.
   */
  deliver(
    assignmentId: number,
    key: string,
    prompt: string,
    onStart: (turnId: string) => void,
    onComplete?: (result: TurnResult) => void | Promise<void>,
  ): boolean {
    if (this.#queued.has(key)) return false;
    this.#queued.add(key);
    void this.#enqueue(assignmentId, () => this.#turn(assignmentId, prompt, (turnId) => {
      onStart(turnId);
      this.#queued.delete(key);
    }, onComplete, key)).finally(() => this.#queued.delete(key));
    return true;
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
      if (assignment.turnStartedAt === null) continue;
      store.recordInterruption(assignment.id);
      pass.interrupted.push(assignment.id);
      void this.#enqueue(assignment.id, () => this.#turn(assignment.id, INTERRUPTED_PROMPT));
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
        writableDirs: [gitDir(this.#deps.config.repo)],
      });
      this.#sessions.set(assignment.id, session);
    }
    return session;
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
    let result: TurnResult;
    try {
      result = await session.send(prompt);
    } catch (error) {
      if (store.getAssignment(id).state === "pending") {
        store.abandonAssignment(id, error instanceof Error ? error.message : String(error));
      } else {
        this.#failed(id, error);
      }
      return;
    }
    this.#finished(id, result);
  }

  async #turn(
    id: number,
    prompt: string,
    onStart?: (turnId: string) => void,
    onComplete?: (result: TurnResult) => void | Promise<void>,
    key = "",
  ): Promise<void> {
    const { store } = this.#deps;
    const assignment = store.getAssignment(id);
    if (assignment.state !== "running") return;
    const ticket = store.getTicket(assignment.ticketId);
    if (ticket.state === "paused" && !key.startsWith("budget-pause-") && !key.startsWith("stuck-")) return;
    const session = this.#session(assignment);
    const turn = store.beginTurn(id);
    onStart?.(`${assignment.sessionId}:${turn}`);
    let result: TurnResult;
    try {
      result = await session.send(prompt);
    } catch (error) {
      this.#failed(id, error);
      return;
    }
    this.#finished(id, result);
    if (result.status === "completed") await onComplete?.(result);
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
    this.#mark(id, result.status === "cancelled" ? "cancelled" : null);
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
