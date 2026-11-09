// Thread subscription (docs/direction.md, milestone 1 step 4): a new trigger on a thread that an
// open ticket is subscribed to goes straight to that ticket's Worker, with no decision and without
// reaching the Router. Delivery is `t3 thread send` without `--wait` (open question 1): T3 queues
// the message as the Worker's next turn. Everything here is deterministic.
//
// A trigger whose subscribed tickets have all ended is not delivered: it stays undecided and goes
// to the Router (`Store.undecidedTriggers`), because whether a message on finished work asks for
// more work is an accept/ignore judgement, not a routing step (AGENTS.md rule 3).

import type { Store, Thread, Trigger } from "../store/store.ts";
import type { T3SendDelivery, T3SendResult } from "../t3/cli-json.ts";
import type { T3Cli } from "../t3/runner.ts";

export interface Delivered {
  triggerId: number;
  ticketId: number;
  assignmentId: number;
  t3ThreadId: string;
  t3RunId: string;
  delivery: T3SendDelivery;
}

export interface RoutePass {
  delivered: Delivered[];
  /** Follow-ups not delivered this pass; each is tried again on the next one. */
  failed: { triggerId: number; ticketId: number; error: string }[];
}

/**
 * One pass over the follow-ups owed to Workers. Each is sent and only then recorded, so a failed
 * send is reported and retried by the next pass, and a replayed pass sends nothing twice.
 */
export async function routeFollowUps(store: Store, t3: T3Cli): Promise<RoutePass> {
  const pass: RoutePass = { delivered: [], failed: [] };
  for (const pending of store.pendingDeliveries()) {
    const { trigger, ticketId, assignmentId, t3ThreadId } = pending;
    if (assignmentId === null || t3ThreadId === null) {
      pass.failed.push({ triggerId: trigger.id, ticketId, error: `ticket ${ticketId} has no running Worker` });
      continue;
    }
    let sent: T3SendResult;
    try {
      sent = await t3.sendMessage(t3ThreadId, followUpMessage(ticketId, trigger, store.getThread(trigger.threadId)));
    } catch (error) {
      pass.failed.push({
        triggerId: trigger.id,
        ticketId,
        error: error instanceof Error ? error.message : String(error),
      });
      continue;
    }
    store.recordDelivery(trigger.id, assignmentId, sent.runId);
    pass.delivered.push({
      triggerId: trigger.id,
      ticketId,
      assignmentId,
      t3ThreadId,
      t3RunId: sent.runId,
      delivery: sent.delivery,
    });
  }
  return pass;
}

const KIND_NOUNS: Record<string, string> = {
  issue_comment: "comment",
  pull_request_review_comment: "review comment",
  pull_request_review: "review",
};

/**
 * The follow-up a Worker receives: a summary plus a pointer ("Inline & Spill"). It never copies
 * the comment body; the Worker reads it with `gh`.
 */
export function followUpMessage(ticketId: number, trigger: Trigger, thread: Thread): string {
  const noun = KIND_NOUNS[trigger.kind] ?? trigger.kind;
  const on = thread.kind === "issue" ? "issue" : "pull request";
  return [
    `Follow-up for Ostrom ticket ${ticketId}: a new ${noun} (${trigger.kind}) by @${trigger.actor}`,
    `on ${on} #${thread.number}: ${trigger.url}`,
    "",
    "Read it with `gh`; this message does not copy it. If it asks for changes, make them and push",
    "to the branch of your existing pull request; do not open a new one. Do not comment on GitHub",
    "yourself: Ostrom replies to people.",
  ].join("\n");
}
