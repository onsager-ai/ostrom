// Derives the current quiet status card from durable ticket and assignment state. Worker plans are
// not yet persisted by the native runners, so their latest todo is deliberately omitted for now.

import { budgetWarning } from "../budget/budgets.ts";
import type { Store } from "../store/store.ts";
import { renderStatusCard, type StatusCardState } from "./status-card.ts";

export function queueStatusCard(store: Store, ticketId: number, now = new Date()): void {
  const context = store.ticketContext(ticketId);
  const ticket = context.ticket;
  const assignment = store.latestAssignment(ticketId);
  if (assignment === null || assignment.harness === null || assignment.model === null) return;
  const openStall = store.unansweredStalls().find((stall) => stall.assignmentId === assignment.id);
  const candidate = store.mergeCandidateForAssignment(assignment.id);
  const policy = candidate === null ? null : JSON.parse(candidate.policy) as { strategy: string };
  const state: StatusCardState =
    ticket.state === "ended" ? (assignment.outcome === "failed" ? "failed" : assignment.endReason === null ? "done" : "stopped") :
      ticket.state === "paused" && ticket.pauseCause?.startsWith("budget reached:") ? "paused" :
      openStall !== undefined && openStall.publicAt !== null ? "stuck" :
        openStall !== undefined ? "recovering" :
          ticket.state === "paused" ? "paused" :
            assignment.outcome === "failed" ? "stuck" :
              candidate !== null ? (policy?.strategy === "human" ? "waiting_for_human_merge" : "waiting_to_merge") : "working";
  const budget = store.ticketBudget(ticketId);
  const warning = budgetWarning(budget, now);
  const pullRequest = store.ticketPullRequest(ticketId);
  const lastActivityAt = new Date(assignment.lastActivityAt ?? assignment.outcomeAt ?? assignment.startedAt ?? assignment.createdAt);
  const turns = store.ticketTurns(ticketId);
  const turn = turns.at(-1);
  const selections = store.selections.history(ticketId);
  const modelOverride = selections.findLast((choice) => choice.kind === "model" && choice.source === "override");
  const effortOverride = selections.findLast((choice) => choice.kind === "effort" && choice.source === "override");
  const modelBoundary = selections.findLast((choice) => choice.kind === "model" && choice.value === `${assignment.harness}/${assignment.model}`);
  const pendingModel = modelOverride?.value && modelOverride.id > (modelBoundary?.id ?? 0) ? modelOverride.value.split("/").at(-1) : undefined;
  const effortBoundary = selections.findLast((choice) => choice.kind === "effort" && choice.source === "router");
  const startsThisTurn = effortOverride?.reason.includes("Applies from this turn") ?? false;
  const pendingEffort = effortOverride && !startsThisTurn && effortOverride.id > (effortBoundary?.id ?? 0) && effortOverride.value !== turn?.requestedEffort ? effortOverride.value : undefined;
  const modelChoice = modelBoundary;
  store.enqueueStatusCard(
    context.thread.id,
    renderStatusCard({
      state,
      ...(assignment.endReason === "issue closed" ? { stoppedReason: "issue closed" as const } : {}),
      ...(pullRequest === null ? {} : { pullRequest: pullRequest.url }),
      ...(warning === null ? {} : { budgetWarning: warning }),
      ...(ticket.pauseCause === null ? {} : { pauseCause: ticket.pauseCause }),
      ...(candidate === null || ticket.state === "ended" ? {} : { mergeStatus: `policy ${policy!.strategy}; head ${candidate.headSha}; ${candidate.blockingReason === "quiet period has not elapsed" && candidate.quietDeadline !== null ? `earliest merge ${candidate.quietDeadline}` : candidate.blockingReason === null ? "waiting for ready reply" : candidate.blockingReason}` }),
      harness: assignment.harness,
      model: assignment.model,
      ...(pendingModel === undefined ? {} : { pendingModel }),
      ...(turn === undefined ? {} : { effort: turn.requestedEffort }),
      ...(pendingEffort === undefined ? {} : { pendingEffort }),
      ...(!startsThisTurn && !turn?.reason && !modelChoice?.reason ? {} : { selectionReason: startsThisTurn ? effortOverride!.reason : turn?.reason ?? modelChoice!.reason }),
      lastActivityAt,
      now,
      markerId: `card-thread-${context.thread.id}`,
    }),
  );
}
