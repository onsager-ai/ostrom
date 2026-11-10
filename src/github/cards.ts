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
    ticket.state === "ended" ? (assignment.endReason === null ? "done" : "stopped") :
      openStall !== undefined && openStall.publicAt !== null ? "stuck" :
        openStall !== undefined ? "recovering" :
          ticket.state === "paused" ? "paused" :
            assignment.outcome === "failed" ? "stuck" :
              candidate !== null ? (policy?.strategy === "human" ? "waiting_for_human_merge" : "waiting_to_merge") : "working";
  const budget = store.ticketBudget(ticketId);
  const warning = budgetWarning(budget, now);
  const pullRequest = store.ticketPullRequest(ticketId);
  const lastActivityAt = new Date(assignment.lastActivityAt ?? assignment.outcomeAt ?? assignment.startedAt ?? assignment.createdAt);
  store.enqueueStatusCard(
    context.thread.id,
    renderStatusCard({
      state,
      ...(assignment.endReason === "issue closed" ? { stoppedReason: "issue closed" as const } : {}),
      ...(pullRequest === null ? {} : { pullRequest: pullRequest.url }),
      ...(warning === null ? {} : { budgetWarning: warning }),
      ...(ticket.pauseCause === null ? {} : { pauseCause: ticket.pauseCause }),
      ...(candidate === null ? {} : { mergeStatus: `policy ${policy!.strategy}; head ${candidate.headSha}; ${candidate.blockingReason === "quiet period has not elapsed" && candidate.quietDeadline !== null ? `earliest merge ${candidate.quietDeadline}` : candidate.blockingReason ?? "waiting for ready reply"}` }),
      harness: assignment.harness,
      model: assignment.model,
      lastActivityAt,
      now,
      markerId: `card-thread-${context.thread.id}`,
    }),
  );
}
