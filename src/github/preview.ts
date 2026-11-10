// Markdown preview of the next outbox send pass. It is deliberately read-only: the same pending
// records and batching rules feed the real sender, so developers can inspect delivery before a tick.

import { budgetReached } from "../budget/budgets.ts";
import type { Reply, Store } from "../store/store.ts";
import type { GhRunner } from "./gh.ts";
import { mergeEligibility, type MergePolicy } from "./merge-policy.ts";
import { readMergeEvidence } from "./merge-reader.ts";

export function renderPreview(store: Store, now = new Date()): string {
  const rows = store.pendingReplies();
  if (rows.length === 0) return "# Ostrom preview\n\nNothing to send.\n";
  return ["# Ostrom preview", "", ...rows.map((reply) => render(reply, store, now))].join("\n\n");
}

function render(reply: Reply, store: Store, now: Date): string {
  const thread = store.getThread(reply.threadId);
  const destination = `${thread.url} (${thread.kind} #${thread.number})`;
  if (reply.kind === "comment") return `## Comment on ${destination}\n\n${reply.body}`;
  if (reply.kind === "edit") return `## Edit status card on ${destination}\n\n${reply.body}`;
  if (reply.kind === "reaction") return `## React ${reply.reaction} on ${reply.target}\n\nReceived ${minutesAgo(reply.enqueuedAt, now)} min ago.`;
  return `## Label on ${destination}\n\n${reply.body}`;
}

function minutesAgo(at: string, now: Date): number {
  return Math.max(0, Math.floor((now.getTime() - Date.parse(at)) / 60_000));
}

/** A direct, read-only report for live tickets with an observed PR. */
export async function renderMergeEligibilityPreview(
  store: Store,
  gh: GhRunner,
  repo: string,
  policy: MergePolicy,
  now = new Date(),
): Promise<string> {
  const reports: string[] = [];
  for (const ticket of store.operatorTickets()) {
    if (ticket.assignment === null || ticket.assignment.branch === null) continue;
    const pr = store.ticketPullRequest(ticket.id);
    if (pr === null) continue;
    try {
      const evidence = await readMergeEvidence(gh, repo, pr.number);
      const result = mergeEligibility({
        evidence, policy, expected: { head: evidence.pr.head.sha, base: evidence.pr.base.sha },
        local: {
          issueOpen: true, ticketOpen: ticket.state === "open",
          pausedOrStopping: ticket.state === "paused", // stop requests arrive with PR 2's durable state.
          stalledOrFailed: ticket.assignment.outcome === "failed" || ticket.assignment.stallRecoveryAttempts > 0,
          budgetReached: budgetReached(ticket.budget, now) !== null,
          workerIdle: ticket.assignment.turnStartedAt === null,
          readyReplySent: false, quietUntil: null, now,
        },
      });
      const untracked = [
        "issue open state not tracked yet", "stop-request state not tracked yet",
        "quiet window not tracked yet", "durable queued-work idle state not tracked yet",
      ];
      reports.push(`## Ticket ${ticket.id}, PR #${pr.number}\n\n${result.eligible ? "observed eligible on tracked evidence" : `blocked: ${result.reason}`}\n\n${untracked.join("; ")}.`);
    } catch (error) {
      reports.push(`## Ticket ${ticket.id}, PR #${pr.number}\n\nblocked: evidence read failed: ${error instanceof Error ? error.message : String(error)}`);
    }
  }
  return [renderPreview(store, now), ...(reports.length === 0 ? ["## Merge eligibility\n\nNo live ticket has an observed pull request."] : ["## Merge eligibility", ...reports])].join("\n\n");
}
