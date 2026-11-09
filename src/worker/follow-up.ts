// Thread subscription (docs/direction.md, milestone 1 step 4): a new trigger on a thread that an
// open ticket is subscribed to goes straight to that ticket's Worker, with no decision and without
// reaching the Router. Delivery is a resumed turn on the Worker's session, queued behind any turn
// that is running. Everything here is deterministic.
//
// A trigger whose subscribed tickets have all ended is not delivered: it stays undecided and goes
// to the Router (`Store.undecidedTriggers`), because whether a message on finished work asks for
// more work is an accept/ignore judgement, not a routing step (AGENTS.md rule 3).

import type { Store, Thread, Trigger } from "../store/store.ts";
import type { Workers } from "./workers.ts";

export interface Delivered {
  triggerId: number;
  ticketId: number;
  assignmentId: number;
}

export interface RoutePass {
  /** Follow-ups queued on a Worker's session this pass; each is recorded when its turn starts. */
  queued: Delivered[];
  /** Follow-ups not queued this pass; each is tried again on the next one. */
  failed: { triggerId: number; ticketId: number; error: string }[];
}

/**
 * One pass over the follow-ups owed to Workers. Each is queued behind the session's running turn
 * and recorded only when its turn starts, so a follow-up lost with the queue in a crash is sent
 * again, one already queued is not queued twice, and a replayed pass delivers nothing twice.
 */
export function routeFollowUps(store: Store, workers: Workers): RoutePass {
  const pass: RoutePass = { queued: [], failed: [] };
  for (const pending of store.pendingDeliveries()) {
    const { trigger, ticketId, assignmentId } = pending;
    if (assignmentId === null) {
      pass.failed.push({ triggerId: trigger.id, ticketId, error: `ticket ${ticketId} has no running Worker` });
      continue;
    }
    const added = workers.deliver(
      assignmentId,
      `${trigger.id}:${assignmentId}`,
      followUpMessage(ticketId, trigger, store.getThread(trigger.threadId)),
      (turnId) => store.recordDelivery(trigger.id, assignmentId, turnId),
    );
    if (added) pass.queued.push({ triggerId: trigger.id, ticketId, assignmentId });
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
