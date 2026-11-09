// Worker dispatch (docs/direction.md, milestone 1 step 3): start a T3 Code Worker for an accepted
// ticket, then follow it through T3 thread state until its PR exists (open question 2, option B).
// Everything here is deterministic; no model call is made by Ostrom itself.

import type { WorkerConfig } from "../ostromd/config.ts";
import type { AssignmentOutcome, Store, TicketContext } from "../store/store.ts";
import type { T3Thread } from "../t3/cli-json.ts";
import type { T3Cli } from "../t3/runner.ts";

export interface Dispatched {
  assignmentId: number;
  t3ThreadId: string;
  branch: string;
}

/**
 * Creates an assignment for an open ticket and starts its Worker. If T3 does not start it, the
 * assignment ends with the error as its reason and the error is rethrown.
 */
export async function dispatch(
  store: Store,
  t3: T3Cli,
  worker: WorkerConfig,
  ticketId: number,
): Promise<Dispatched> {
  const context = store.ticketContext(ticketId);
  const assignmentId = store.createAssignment(ticketId);
  const branch = `ostrom/ticket-${ticketId}-${assignmentId}`;
  let t3ThreadId: string;
  try {
    ({ threadId: t3ThreadId } = await t3.startThread({
      project: worker.project,
      title: `Ostrom ticket ${ticketId}: ${context.source.name}#${context.thread.number}`,
      brief: workerBrief(context, branch, worker.base),
      worktreeBase: worker.base,
      branch,
      provider: worker.provider,
      model: worker.model,
    }));
  } catch (error) {
    store.abandonAssignment(assignmentId, error instanceof Error ? error.message : String(error));
    throw error;
  }
  store.startAssignment(assignmentId, t3ThreadId);
  return { assignmentId, t3ThreadId, branch };
}

/**
 * The Worker's first message: a summary plus pointers ("Inline & Spill"). It quotes no comment
 * bodies; the Worker reads the thread itself.
 */
export function workerBrief(context: TicketContext, branch: string, base: string): string {
  const { ticket, thread, source } = context;
  const noun = thread.kind === "issue" ? "issue" : "pull request";
  return [
    `You are the Worker for Ostrom ticket ${ticket.id} on ${source.name}.`,
    "",
    `Request, as the Router summarised it: ${context.reason}`,
    `GitHub ${noun} #${thread.number}: ${thread.url}`,
    "Comments that asked for the work:",
    ...context.triggerUrls.map((url) => `- ${url}`),
    "",
    "Read that thread and those comments with `gh` for the full request; this message does not copy them.",
    `Do the work in this worktree on branch ${branch}. Commit, push the branch, and open a pull request`,
    `on ${source.name} against ${base} whose description says "Refs #${thread.number}".`,
    "Do not comment on GitHub yourself: Ostrom replies to people. Review comments on your pull",
    "request will reach you in this thread.",
  ].join("\n");
}

export interface Observed {
  assignmentId: number;
  t3ThreadId: string;
  /** Pull requests on the repo the thread shows, by number. */
  pullRequests: number[];
  /** Of those, the ones this pass subscribed the ticket to for the first time. */
  newlySubscribed: number[];
  outcome: AssignmentOutcome | null;
}

export interface ObservePass {
  observed: Observed[];
  failed: { assignmentId: number; error: string }[];
}

/**
 * One pass over running assignments: read each Worker's T3 thread, subscribe its ticket to the
 * PRs it opened on `repo`, and record an outcome worth flagging. A thread that cannot be read is
 * reported and the pass goes on to the next one.
 */
export async function observe(store: Store, t3: T3Cli, repo: string): Promise<ObservePass> {
  const source = store.ensureSource("github", repo);
  const pass: ObservePass = { observed: [], failed: [] };
  for (const assignment of store.runningAssignments()) {
    const t3ThreadId = assignment.t3ThreadId!;
    let thread: T3Thread;
    try {
      ({ thread } = await t3.showThread(t3ThreadId));
    } catch (error) {
      pass.failed.push({
        assignmentId: assignment.id,
        error: error instanceof Error ? error.message : String(error),
      });
      continue;
    }
    const pullRequests = pullRequestsOn(thread, repo);
    const newlySubscribed: number[] = [];
    for (const pr of pullRequests) {
      const threadId = store.ensureThread({
        sourceId: source.id,
        kind: "pull_request",
        number: pr.number,
        url: pr.url,
      });
      if (!store.subscribe(assignment.ticketId, threadId).duplicate) newlySubscribed.push(pr.number);
    }
    const outcome = outcomeOf(thread);
    store.recordOutcome(assignment.id, outcome);
    pass.observed.push({
      assignmentId: assignment.id,
      t3ThreadId,
      pullRequests: pullRequests.map((pr) => pr.number),
      newlySubscribed,
      outcome,
    });
  }
  return pass;
}

/** The thread's PRs on `repo`, linked or listed, by number; ones the user dismissed are left out. */
export function pullRequestsOn(thread: T3Thread, repo: string): { number: number; url: string }[] {
  const same = (repository: string) => repository.toLowerCase() === repo.toLowerCase();
  const found = new Map<number, string>();
  if (thread.linkedPullRequest !== null && same(thread.linkedPullRequest.repository)) {
    found.set(thread.linkedPullRequest.number, thread.linkedPullRequest.url);
  }
  for (const pr of thread.pullRequests) {
    if (pr.host === "github.com" && same(pr.repository) && pr.source !== "stack-dismissed") {
      if (!found.has(pr.number)) found.set(pr.number, pr.url);
    }
  }
  return [...found].sort(([a], [b]) => a - b).map(([number, url]) => ({ number, url }));
}

export function outcomeOf(thread: T3Thread): AssignmentOutcome | null {
  if (thread.status === "failed" || thread.status === "cancelled") return thread.status;
  if (thread.status === "waiting" && thread.pendingRequestCount > 0) return "waiting";
  return null;
}
