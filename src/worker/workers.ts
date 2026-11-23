// Native Workers (docs/direction.md, milestone 1 PR 2): each Worker is a Claude Code or Codex
// session started and resumed by ID through the session runner. Turns on one session run one at a
// time; a follow-up queues behind the running turn. Everything here is deterministic.

import { randomUUID } from "node:crypto";
import { basename, join } from "node:path";

import { turnBudgetReached } from "../budget/budgets.ts";
import { queueStatusCard } from "../github/cards.ts";
import { ghGet, type GhRunner } from "../github/gh.ts";
import { renderReviewReply } from "../github/review-reply.ts";
import { parseStuckReply, renderStuckReply } from "../watchdog/stuck.ts";
import { chooseEffort, sessionModel } from "../router/selection.ts";
import type { ThreadContext } from "../router/thread-context.ts";
import type { RouterConfig, WorkerConfig } from "../ostromd/config.ts";
import { appendSessionAnnotationIfChanged } from "../session/annotations.ts";
import { rolloutContext } from "../session/rollout.ts";
import type { Effort, Runner, Session, TurnResult } from "../session/runner.ts";
import { createWorktree } from "../session/worktree.ts";
import type { Assignment, NewReply, QueuedTurnKind, Store } from "../store/store.ts";
import { pullRequestsForBranch, workerBrief } from "./dispatch.ts";

export interface Dispatched {
  assignmentId: number;
  sessionId: string;
  branch: string;
}

export interface WorkersDeps {
  store: Store;
  config: WorkerConfig;
  router?: RouterConfig;
  runners: Record<"claude" | "codex", Runner>;
  /** Where Codex writes rollout records; set to verify the Worker's model after each Codex turn. */
  codexSessions?: string;
  addWorktree?: typeof createWorktree;
  log?: (line: string) => void;
  /** GitHub access verifies turn delivery and restores stop cleanup after a daemon restart. */
  gh?: GhRunner;
  repo?: string;
  semonUrl?: string;
  /** Fresh issue or PR context for per-turn effort choices. */
  threadContext?: (ticketId: number) => Promise<ThreadContext | undefined>;
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
  "The previous turn was interrupted by a daemon stop. Its work may be partly pushed. " +
  "Check git status, the current PR head and CI before acting. " +
  "The interrupted prompt and any newer queued feedback follow in order.";

export function workerTurnPrompt(prompt: string): string {
  const contract = "Do not merge, enable auto-merge, dismiss reviews, or resolve human review threads.";
  return prompt.includes(contract) ? prompt : `${prompt}\n\n${contract}`;
}

function queuedTurnKind(key: string): QueuedTurnKind {
  if (key.startsWith("review-")) return "review";
  if (key.startsWith("conflict-")) return "conflict";
  if (key.startsWith("ci-fix-")) return "ci_fix";
  if (key.startsWith("budget-pause-")) return "budget_pause";
  if (key.startsWith("stuck-")) return "stuck";
  if (key.startsWith("recovery-")) return "recovery";
  if (/^(operator-|pause-|resume-|stop-|close-issue-)/.test(key)) return "control";
  return "follow_up";
}

interface TurnCompletion { result: TurnResult; blocked: boolean; pushedHead: string | null; }

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
    const selected = sessionModel(store, ticketId, config);
    const id = store.createAssignment(ticketId);
    try {
      const suffix = randomUUID();
      const path = join(config.worktrees, `ticket-${ticketId}-${id}-${suffix}`);
      const branch = await (this.#deps.addWorktree ?? createWorktree)(config.repo, path, config.base, ticketId, id, suffix);
      store.configureNativeAssignment(id, { harness: selected.harness, model: selected.model, worktree: path, branch, machine: config.machine });
      let resolve!: (sessionId: string) => void;
      const started = new Promise<string>((r) => (resolve = r));
      const session = this.#deps.runners[selected.harness].start({
        cwd: path,
        model: selected.model,
        writableDirs: gitDirs(config.repo, path),
        onActivity: () => { if (!this.#stopping) store.recordWorkerActivity(id); },
        onSessionId: (sessionId) => {
          if (this.#stopping) return;
          store.startNativeAssignment(id, sessionId);
          this.#annotate(id, sessionId, null, null);
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

  /** The merge pass rechecks the automatic-turn controls before a staged run result can start. */
  wakeDispatchedRun(assignmentId: number, key: string, allowed: () => boolean): void {
    void this.#enqueue(assignmentId, async () => {
      if (allowed()) await this.#turn(assignmentId, key);
    });
  }

  /** Cancels the turn currently running for an assignment, if any. */
  cancel(assignmentId: number): boolean {
    const controller = this.#inFlight.get(assignmentId);
    if (controller === undefined || controller.signal.aborted) return false;
    controller.abort();
    return true;
  }

  /** Stops SDK turns and records their stop time; the assignment marker requests recovery next start. */
  stop(): void {
    this.#stopping = true;
    for (const [id, controller] of this.#inFlight) {
      this.#deps.store.finishWorkerTurn(id, "interrupted", null, undefined, "ostromd stopped during the turn");
      controller.abort();
    }
  }

  /** SDK cancellation may stall; the daemon must still release its store and exit. */
  async shutdown(timeoutMs = 5_000): Promise<boolean> {
    this.stop();
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      return await Promise.race([
        this.drain().then(() => true),
        new Promise<false>((resolve) => { timer = setTimeout(() => resolve(false), timeoutMs); }),
      ]);
    } finally { clearTimeout(timer); }
  }

  /**
   * Run once at start. A native assignment that never got a session ends; the others resume by
   * session ID, and one whose turn was cut off is sent a turn to check its state and go on.
   */
  recover(): { resumed: number[]; interrupted: number[]; abandoned: number[] } {
    const { store } = this.#deps;
    const pass = { resumed: [] as number[], interrupted: [] as number[], abandoned: [] as number[] };
    for (const pending of store.pendingAssignments()) {
      store.interruptRecoveredTurn(pending.id, "ostromd stopped before the session started");
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
        const interrupted = store.unfinishedTurns(assignment.id).filter((turn) => turn.state === "running");
        const prompt = [INTERRUPTED_PROMPT, ...interrupted.map((turn) => turn.prompt)].join("\n\n");
        store.interruptRecoveredTurn(assignment.id, "ostromd stopped during the turn");
        store.recordInterruption(assignment.id);
        store.finishRunningTurns(assignment.id);
        store.endTurn(assignment.id, null);
        pass.interrupted.push(assignment.id);
        this.deliver(assignment.id, `recovery-${assignment.interruptions + 1}`, prompt, (recoveryTurnId) => {
          store.recoverDeliveries(assignment.id, turnId, recoveryTurnId);
        }, (result) => {
          for (const review of reviews) {
            store.enqueueReply({
              id: `review-${assignment.id}-${review.reviewId}`,
              threadId: review.threadId,
              body: renderReviewReply(result.text),
              blocking: true,
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
        onActivity: () => { if (!this.#stopping) this.#deps.store.recordWorkerActivity(assignment.id); },
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
    // Workflow results wait for the merge pass, including after resume or a daemon restart.
    const queued = this.#deps.store.unfinishedTurns(id).filter((turn) => turn.state === "queued" && turn.kind !== "dispatched_run");
    for (const turn of queued.filter((turn) => turn.kind === "recovery" && turn.sourcePointer !== null)) {
      this.#schedule(id, turn.deliveryKey);
    }
    for (const turn of queued.filter((turn) => turn.kind !== "recovery" || turn.sourcePointer === null)) {
      this.#schedule(id, turn.deliveryKey);
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
          store.enqueueReply({ id: `review-${id}-${review.reviewId}`, threadId: review.threadId, body: renderReviewReply(result.text), blocking: true });
        },
      };
    }
    if (turn.kind === "control") return this.#restoredControl(id, key);
    if (turn.kind === "budget_pause") return { onComplete: () => queueStatusCard(store, store.getAssignment(id).ticketId, new Date(), this.#deps.semonUrl) };
    if (turn.kind === "stuck") return this.#restoredStuck(id, key);
    if (turn.kind === "recovery" && turn.sourcePointer !== null) {
      const interruptedTurnId = turn.sourcePointer;
      const reviews = store.reviewDeliveries(id, interruptedTurnId);
      return {
        onStart: (recoveryTurnId) => store.recoverDeliveries(id, interruptedTurnId, recoveryTurnId),
        onComplete: (result) => {
          for (const review of reviews) {
            store.enqueueReply({ id: `review-${id}-${review.reviewId}`, threadId: review.threadId, body: renderReviewReply(result.text), blocking: true });
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
        queueStatusCard(store, assignment.ticketId, new Date(), this.#deps.semonUrl);
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
        queueStatusCard(store, assignment.ticketId, new Date(), this.#deps.semonUrl);
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
    if (this.#stopping) return;
    const { store } = this.#deps;
    const controller = new AbortController();
    this.#inFlight.set(id, controller);
    let completed: TurnCompletion;
    try {
      const choice = await this.#effort(id, "dispatch", prompt, controller.signal);
      if (controller.signal.aborted) throw new Error("Turn cancelled before Worker start");
      store.startWorkerTurn(id, "dispatch", choice.effort, this.#selectionReason(id, choice.reason));
      completed = await this.#send(id, session, "dispatch", prompt, controller.signal, choice.effort);
    } catch (error) {
      if (this.#stopping) return;
      if (controller.signal.aborted) {
        store.finishWorkerTurn(id, "cancelled", null);
        store.endTurn(id, null);
        store.clearTurnError(id);
      } else if (store.getAssignment(id).state === "pending") {
        store.finishWorkerTurn(id, "failed", null, undefined, error instanceof Error ? error.message : String(error));
        store.abandonAssignment(id, error instanceof Error ? error.message : String(error));
      } else {
        this.#failed(id, error);
      }
      return;
    } finally {
      if (this.#inFlight.get(id) === controller) this.#inFlight.delete(id);
    }
    if (this.#stopping) return;
    this.#finished(id, completed);
  }

  async #turn(
    id: number,
    key: string,
    onStart?: (turnId: string) => void,
    onComplete?: (result: TurnResult) => void | Promise<void>,
  ): Promise<void> {
    if (this.#stopping) return;
    const { store } = this.#deps;
    const assignment = store.getAssignment(id);
    const queued = store.queuedTurn(id, key);
    if (queued === null || queued.state !== "queued" || this.#stopping || assignment.state !== "running" || assignment.outcome === "failed") return;
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
    // Recovery leads queued feedback so stale work and newer reviews reach one native turn.
    // Controls retain their own boundaries and side effects.
    const folded = queued.kind === "recovery" && queued.sourcePointer !== null
      ? store.unfinishedTurns(id).filter((turn) => turn.state === "queued" &&
        ["follow_up", "review", "conflict", "ci_fix"].includes(turn.kind))
      : [];
    const deliveries = folded.map((turn) => ({
      turn,
      callbacks: this.#callbacksByKey.get(this.#callbackKey(id, turn.deliveryKey)) ?? this.#callbacks(id, turn.deliveryKey),
    }));
    for (const delivery of deliveries) {
      if (store.beginQueuedTurn(id, delivery.turn.deliveryKey, turnId) === null) {
        throw new Error(`assignment ${id}: recovery delivery could not be claimed`);
      }
    }
    const prompt = [queued.prompt, ...folded.map((turn) => turn.prompt)].join("\n\n");
    const finish = () => {
      store.finishQueuedTurn(id, key);
      for (const delivery of deliveries) store.finishQueuedTurn(id, delivery.turn.deliveryKey);
    };
    const turn = store.beginTurn(id);
    if (turn !== nextTurn) throw new Error(`assignment ${id}: queued turn number changed`);
    if (assignment.outcome === "waiting") queueStatusCard(store, assignment.ticketId, new Date(), this.#deps.semonUrl);
    const kind = key.startsWith("stuck-recovery-") ? "recovery" : key.startsWith("redirect-") ? "redirect" : queued.kind;
    onStart?.(turnId);
    for (const delivery of deliveries) delivery.callbacks.onStart?.(turnId);
    const controller = new AbortController();
    this.#inFlight.set(id, controller);
    let completed: TurnCompletion;
    try {
      const choice = await this.#effort(id, kind, prompt, controller.signal);
      if (controller.signal.aborted) throw new Error("Turn cancelled before Worker start");
      store.startWorkerTurn(id, kind, choice.effort, this.#selectionReason(id, choice.reason));
      queueStatusCard(store, assignment.ticketId, new Date(), this.#deps.semonUrl);
      const pauseTurn = kind === "budget_pause" || key.startsWith("pause-") || key.startsWith("operator-pause-");
      completed = await this.#send(id, session, kind, workerTurnPrompt(prompt), controller.signal, choice.effort, pauseTurn);
    } catch (error) {
      if (this.#stopping) return;
      finish();
      if (controller.signal.aborted) {
        store.finishWorkerTurn(id, "cancelled", null);
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
    finish();
    const { result } = completed;
    this.#finished(id, completed);
    const current = store.getAssignment(id);
    if (result.status === "completed" && current.state === "running" && current.outcome !== "failed") {
      await onComplete?.(result);
      for (const delivery of deliveries) await delivery.callbacks.onComplete?.(result);
    }
    this.#callbacksByKey.delete(this.#callbackKey(id, key));
    for (const delivery of deliveries) this.#callbacksByKey.delete(this.#callbackKey(id, delivery.turn.deliveryKey));
  }

  #annotate(id: number, nativeId: string, pr: number | null, title: string | null): void {
    const { store } = this.#deps;
    const context = store.ticketContext(store.getAssignment(id).ticketId);
    try {
      appendSessionAnnotationIfChanged({
        harness: store.getAssignment(id).harness!, native_id: nativeId, system: "ostrom", repository: context.source.name,
        ticket: String(context.ticket.id), issue: context.thread.kind === "issue" ? context.thread.number : null,
        pr, title, updated_at: new Date().toISOString(),
      });
    } catch (error) {
      this.#deps.log?.(`assignment ${id}: could not append Semon annotation: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  #selectionReason(id: number, effortReason: string): string {
    const assignment = this.#deps.store.getAssignment(id);
    const modelChoice = this.#deps.store.selections.history(assignment.ticketId).findLast((choice) => choice.kind === "model" && choice.value === `${assignment.harness}/${assignment.model}`);
    return [modelChoice?.reason, effortReason].filter(Boolean).join(" ");
  }

  #effort(id: number, kind: string, prompt: string, signal: AbortSignal) {
    if (!this.#deps.router) return Promise.resolve({ effort: null, reason: "Harness default effort (no Router configured)." });
    const options = { store: this.#deps.store, assignmentId: id, kind, prompt, signal, router: this.#deps.router, runners: this.#deps.runners };
    if (!this.#deps.threadContext) return chooseEffort(options);
    return this.#deps.threadContext(this.#deps.store.getAssignment(id).ticketId).then((threadContext) =>
      chooseEffort({ ...options, ...(threadContext === undefined ? {} : { threadContext }) }));
  }

  #failed(id: number, error: unknown): void {
    const message = error instanceof Error ? error.message : String(error);
    this.#deps.log?.(`assignment ${id}: turn failed: ${message}`);
    this.#deps.store.finishWorkerTurn(id, "failed", null, undefined, message);
    this.#deps.store.endTurn(id, null);
    this.#deps.store.recordTurnError(id, message);
    if (this.#deps.store.getAssignment(id).state === "running") this.#deps.store.recordOutcome(id, "failed");
    queueStatusCard(this.#deps.store, this.#deps.store.getAssignment(id).ticketId, new Date(), this.#deps.semonUrl);
  }

  async #send(id: number, session: Session, kind: string, prompt: string, signal: AbortSignal, effort: Effort | null,
    pauseTurn = false): Promise<TurnCompletion> {
    const { store, gh, repo } = this.#deps;
    const { branch, ticketId } = store.getAssignment(id);
    const ticketAllowsRecording = () => {
      const ticket = store.getTicket(ticketId);
      return ticket.state === "open" && ticket.stopRequestedAt === null;
    };
    const pauseCount = store.ticketPauseCount(ticketId);
    const hasRemote = gh !== undefined && repo !== undefined && branch !== null;
    const verifyDelivery = hasRemote && ["dispatch", "follow_up", "redirect"].includes(kind);
    // Undefined means the read failed or was skipped; null is positive evidence that the ref is absent.
    const before = verifyDelivery ? await this.#deliveryEvidence(id, () => branchHead(gh, repo, branch)) : undefined;
    const readPulls = hasRemote ? () => this.#deliveryEvidence(id, () => pullRequestsForBranch(gh, repo, branch)) : undefined;
    const beforePulls = !pauseTurn && ticketAllowsRecording() ? await readPulls?.() : undefined;
    if (signal.aborted) throw new Error("Turn cancelled before Worker start");
    const result = await session.send(prompt, signal, effort);
    if (result.status !== "completed" || signal.aborted) return { result, blocked: false, pushedHead: null };
    const afterPulls = beforePulls !== undefined || (verifyDelivery && before !== undefined) ? await readPulls?.() : undefined;
    const blocked = !verifyDelivery || before === undefined || afterPulls?.length !== 0 ? false :
      await this.#deliveryEvidence(id, async () => {
        const after = await branchHead(gh, repo, branch);
        return after === null || after === before;
      });
    const startPrs = beforePulls?.filter((pr) => pr.state === "open");
    const endPrs = afterPulls?.filter((pr) => pr.state === "open");
    const startPr = startPrs?.[0];
    const endPr = endPrs?.[0];
    // A re-request needs two heads for the same PR. Opening a PR alone is not a head change.
    const pushedHead = !signal.aborted && ticketAllowsRecording() && store.ticketPauseCount(ticketId) === pauseCount &&
      startPrs?.length === 1 && endPrs?.length === 1 && startPr?.headSha !== undefined && endPr?.headSha !== undefined &&
      startPr.number === endPr.number && startPr.headSha !== endPr.headSha
      ? endPr.headSha : null;
    return { result, blocked: blocked === true && !signal.aborted, pushedHead };
  }

  async #deliveryEvidence<T>(id: number, read: () => Promise<T>): Promise<T | undefined> {
    try {
      return await read();
    } catch (error) {
      this.#deps.log?.(`assignment ${id}: could not verify turn delivery: ${error instanceof Error ? error.message : String(error)}`);
      return undefined;
    }
  }

  #finished(id: number, { result, blocked, pushedHead }: TurnCompletion): void {
    const { store } = this.#deps;
    const assignment = store.getAssignment(id);
    let observed = { model: result.model, effort: null as string | null };
    let modelProblem: string | null = null;
    if (assignment.harness === "codex" && this.#deps.codexSessions !== undefined) {
      try {
        observed = rolloutContext(this.#deps.codexSessions, assignment.sessionId!);
        if (observed.model === null) throw new Error("rollout records no turn model");
        if (observed.model !== assignment.model) modelProblem = `model mismatch: asked for ${assignment.model}, ran ${observed.model}`;
      } catch (error) {
        modelProblem = `model unverified: ${error instanceof Error ? error.message : String(error)}`;
      }
    }
    let failure: string | null = null;
    let blockedReply: NewReply | undefined;
    let refreshCard = false;
    try {
      // A late result still counts its usage, but cannot clear a failure or act for an ended assignment.
      store.endTurn(id, result.usage);
      if (assignment.state !== "running" || assignment.outcome === "failed") return;
      store.clearTurnError(id);
      if (blocked && modelProblem === null && store.getTicket(assignment.ticketId).state === "open") {
        blockedReply = {
          id: `blocked-a${id}-turn-${assignment.turns}`,
          threadId: store.ticketContext(assignment.ticketId).thread.id,
          body: ["The Worker stopped without opening a pull request:", result.text.trim() || "The Worker left no final message.",
            "Reply here to redirect, or `@ostrom stop`."].join("\n\n"),
          blocking: true,
        };
      }
      // A cancelled turn still counts toward the turn and token limits.
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
        refreshCard = true;
        return;
      }
      if (store.getTicket(assignment.ticketId).state === "open") this.#budgetPauses.delete(id);
      if (result.status === "cancelled") {
        refreshCard = true;
        return;
      }
      if (modelProblem !== null) {
        failure = modelProblem;
        this.#deps.log?.(`assignment ${id}: ended: ${modelProblem}`);
        store.endAssignment(id, modelProblem);
        return;
      }
      if (blockedReply === undefined) store.recordOutcome(id, null);
      refreshCard = true;
    } finally {
      const ticket = store.getTicket(assignment.ticketId);
      store.finishWorkerTurn(id, failure === null ? result.status : "failed", result.usage, observed,
        failure ?? (result.status === "completed" ? result.text : null), blockedReply,
        ticket.state === "open" && ticket.stopRequestedAt === null ? pushedHead : null);
      if (refreshCard) queueStatusCard(store, assignment.ticketId, new Date(), this.#deps.semonUrl);
    }
  }
}

async function branchHead(gh: GhRunner, repo: string, branch: string): Promise<string | null> {
  // The matching-refs endpoint returns an empty list for an absent branch and can also return
  // longer refs sharing the same prefix. Only the exact branch is delivery evidence.
  const path = branch.split("/").map(encodeURIComponent).join("/");
  const refs = await ghGet(gh, `repos/${repo}/git/matching-refs/heads/${path}`);
  if (!Array.isArray(refs)) throw new Error("expected matching refs");
  const ref = refs.find((value) => value?.ref === `refs/heads/${branch}`);
  if (ref === undefined) return null;
  const sha = ref.object?.sha;
  if (typeof sha !== "string" || sha.length === 0) throw new Error("branch ref has no SHA");
  return sha;
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
