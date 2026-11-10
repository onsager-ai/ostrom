// The Worker brief and observation (docs/direction.md, milestone 1 step 3): Workers start in
// `workers.ts`; here a running Worker is followed through GitHub until its PR exists. Everything
// here is deterministic; no model call is made by Ostrom itself.

import { queueStatusCard } from "../github/cards.ts";
import { ghGet, type GhRunner } from "../github/gh.ts";
import type { AssignmentOutcome, Store, TicketContext } from "../store/store.ts";

/**
 * The Worker's first message: a summary plus pointers ("Inline & Spill"). It quotes no comment
 * bodies; the Worker reads the thread itself.
 */
export function workerBrief(context: TicketContext, branch: string, base: string, rules?: string): string {
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
    "Do not comment on GitHub yourself: Ostrom replies to people. After the quiet window Ostrom may",
    "squash-merge your pull request, so do not merge it, enable auto-merge, dismiss reviews, or resolve",
    "human review threads yourself. Review comments on your pull request will reach you in this session as follow-up messages.",
    "You run unattended: never stop to ask an interactive question. Decide, state your assumption in",
    "the pull request description, and carry on.",
    "If a normal commit fails, report the failure; do not create or push commit objects with Git plumbing.",
    ...(rules === undefined ? [] : ["", "House rules for this machine, which you must follow:", rules]),
  ].join("\n");
}

export type PullRequestState = "open" | "closed" | "merged";

export interface BranchPullRequest {
  number: number;
  url: string;
  state: PullRequestState;
}

/** The pull requests on `repo` whose head is `branch`, in any state, by number. */
export async function pullRequestsForBranch(gh: GhRunner, repo: string, branch: string): Promise<BranchPullRequest[]> {
  const owner = repo.split("/")[0]!;
  const found = await ghGet(gh, `repos/${repo}/pulls?head=${encodeURIComponent(`${owner}:${branch}`)}&state=all&per_page=100`);
  if (!Array.isArray(found)) throw new Error(`gh pulls for ${branch}: expected a list`);
  return found
    .map((item: Record<string, unknown>) => {
      if (typeof item["number"] !== "number" || typeof item["html_url"] !== "string") {
        throw new Error(`gh pulls for ${branch}: unexpected item ${JSON.stringify(item)}`);
      }
      const state: PullRequestState = item["merged_at"] != null ? "merged" : item["state"] === "closed" ? "closed" : "open";
      return { number: item["number"], url: item["html_url"], state };
    })
    .sort((a, b) => a.number - b.number);
}

export interface Observed {
  assignmentId: number;
  sessionId: string;
  /** Pull requests from the Worker's branch, by number. */
  pullRequests: number[];
  /** Of those, the ones this pass subscribed the ticket to for the first time. */
  newlySubscribed: number[];
  /** What the Worker's turns recorded (failed or cancelled), or null for nothing to flag. */
  outcome: AssignmentOutcome | null;
}

export interface ObservePass {
  observed: Observed[];
  failed: { assignmentId: number; error: string }[];
}

/**
 * One pass over running Workers: find the PRs opened from each Worker's branch with `gh`,
 * subscribe its ticket to them, and report the outcome its turns recorded. A lookup that fails is
 * reported and the pass goes on to the next one.
 */
export async function observe(store: Store, gh: GhRunner, repo: string): Promise<ObservePass> {
  const source = store.ensureSource("github", repo);
  const pass: ObservePass = { observed: [], failed: [] };
  for (const assignment of store.runningAssignments()) {
    if (assignment.sessionId === null) continue;
    let pullRequests: BranchPullRequest[];
    try {
      pullRequests = await pullRequestsForBranch(gh, repo, assignment.branch!);
    } catch (error) {
      pass.failed.push({
        assignmentId: assignment.id,
        error: error instanceof Error ? error.message : String(error),
      });
      continue;
    }
    const newlySubscribed: number[] = [];
    for (const pr of pullRequests) {
      const threadId = store.ensureThread({ sourceId: source.id, kind: "pull_request", number: pr.number, url: pr.url });
      if (!store.subscribe(assignment.ticketId, threadId).duplicate) newlySubscribed.push(pr.number);
    }
    // The card links the PR, so a new one refreshes it.
    if (newlySubscribed.length > 0) queueStatusCard(store, assignment.ticketId);
    pass.observed.push({
      assignmentId: assignment.id,
      sessionId: assignment.sessionId,
      pullRequests: pullRequests.map((pr) => pr.number),
      newlySubscribed,
      outcome: assignment.outcome,
    });
  }
  return pass;
}
