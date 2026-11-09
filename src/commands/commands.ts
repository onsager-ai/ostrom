import { queueStatusCard } from "../github/cards.ts";
import { ghGet, type GhRunner } from "../github/gh.ts";
import type { Store, Trigger } from "../store/store.ts";
import { followUpMessage } from "../worker/follow-up.ts";
import type { Workers } from "../worker/workers.ts";
import { parseComment } from "./parser.ts";

export interface CommandPass {
  handled: { triggerId: number; ticketId: number; command: string; allowed: boolean }[];
  failed: { triggerId: number; ticketId: number; error: string }[];
}

/** Operator controls share the Worker delivery and durable state transitions used by GitHub commands. */
export function operatorControls(store: Store, workers: Workers, gh: GhRunner, repo: string): {
  pause(ticketId: number): Promise<void>;
  resume(ticketId: number): Promise<void>;
  stop(ticketId: number): Promise<void>;
} {
  const running = (ticketId: number) => {
    const assignment = store.latestAssignment(ticketId);
    if (assignment?.state !== "running") throw new Error(`ticket ${ticketId} has no running Worker`);
    return assignment;
  };
  return {
    async pause(ticketId) {
      const assignment = running(ticketId);
      store.pauseTicket(ticketId);
      queueStatusCard(store, ticketId);
      workers.deliver(assignment.id, `operator-pause-${ticketId}`, "Pause this ticket after the current work finishes. Push your work in progress, then stop and wait for Ostrom to resume you.", () => {});
    },
    async resume(ticketId) {
      const assignment = running(ticketId);
      store.resumeTicket(ticketId);
      queueStatusCard(store, ticketId);
      workers.deliver(assignment.id, `operator-resume-${ticketId}`, "This ticket has resumed. Continue your work.", () => {});
    },
    async stop(ticketId) {
      const assignment = running(ticketId);
      workers.deliver(
        assignment.id,
        `operator-stop-${ticketId}`,
        "Stop this ticket gracefully. Push your work in progress and summarise precisely where you stopped.",
        () => {},
        async (result) => {
          for (const pr of await pullRequests(gh, repo, assignment.branch!)) await gh(["pr", "ready", "--undo", pr]);
          store.endAssignment(assignment.id, "stopped by operator");
          store.endTicket(ticketId);
          store.enqueueReply({ id: `operator-stopped-${ticketId}`, threadId: store.ticketContext(ticketId).thread.id, body: result.text, blocking: true });
          queueStatusCard(store, ticketId);
        },
      );
    },
  };
}

/**
 * Consumes subscribed-thread comments before ordinary follow-up routing. This is the only path
 * that parses control words, so a command is never supplied to the Router or a Worker as text.
 */
export async function routeCommands(
  store: Store,
  workers: Workers,
  gh: GhRunner,
  repo: string,
): Promise<CommandPass> {
  const pass: CommandPass = { handled: [], failed: [] };
  const permissions = new Map<string, Promise<boolean>>();
  const authors = new Map<number, Promise<string>>();
  for (const pending of store.pendingCommands()) {
    const parsed = parseComment(pending.trigger.body ?? "");
    const command = pending.trigger.kind === "issue_state" ?
      (store.getThread(pending.trigger.threadId).kind === "issue" ? "close_issue" : "close_pull_request") :
      parsed.kind === "command" ? parsed.command : "redirect";
    try {
      const allowed = command === "redirect" || command === "close_issue" || command === "close_pull_request" || await mayControl(
        store, gh, repo, pending.trigger, pending.ticketId, permissions, authors,
      );
      if (!allowed) {
        store.recordCommandReceipt(pending.trigger.id, pending.ticketId, "forbidden", false);
        enqueueReaction(store, pending.trigger, "confused");
        pass.handled.push({ triggerId: pending.trigger.id, ticketId: pending.ticketId, command, allowed });
        continue;
      }
      if (command === "close_pull_request") {
        const assignment = store.latestAssignment(pending.ticketId);
        if (assignment?.state !== "running") throw new Error(`ticket ${pending.ticketId} has no running Worker`);
        store.recordCommandReceipt(pending.trigger.id, pending.ticketId, "close_pull_request", true);
        store.unsubscribeTicket(pending.ticketId);
        store.endAssignment(assignment.id, "pull request closed");
        queueStatusCard(store, pending.ticketId);
      } else if (command === "close_issue") {
        const assignment = store.latestAssignment(pending.ticketId);
        if (assignment?.state !== "running") throw new Error(`ticket ${pending.ticketId} has no running Worker`);
        store.recordCommandReceipt(pending.trigger.id, pending.ticketId, "close_issue", true);
        workers.deliver(
          assignment.id,
          `close-issue-${pending.trigger.id}-${assignment.id}`,
          "The issue was closed. Stop this ticket gracefully, push your work in progress, and summarise precisely where you stopped.",
          () => {},
          async (result) => {
            for (const pr of await pullRequests(gh, repo, assignment.branch!)) await gh(["pr", "ready", "--undo", pr]);
            store.endAssignment(assignment.id, "issue closed");
            store.endTicket(pending.ticketId);
            store.enqueueReply({ id: `stopped-issue-closed-${pending.ticketId}`, threadId: pending.trigger.threadId, body: result.text, blocking: true });
            queueStatusCard(store, pending.ticketId);
          },
        );
      } else if (command === "redirect") {
        const assignment = store.latestAssignment(pending.ticketId);
        if (assignment?.state !== "running") throw new Error(`ticket ${pending.ticketId} has no running Worker`);
        const instruction = await collaborator(gh, repo, pending.trigger.actor, permissions);
        store.recordCommandReceipt(pending.trigger.id, pending.ticketId, "redirect", instruction);
        const added = workers.deliver(
          assignment.id,
          `redirect-${pending.trigger.id}-${assignment.id}`,
          followUpMessage(pending.ticketId, pending.trigger, store.getThread(pending.trigger.threadId), instruction),
          (turnId) => {
            store.recordDelivery(pending.trigger.id, assignment.id, turnId);
            enqueueReaction(store, pending.trigger, "eyes");
          },
        );
        if (!added) continue;
      } else if (command === "pause") {
        const assignment = store.latestAssignment(pending.ticketId);
        if (assignment?.state !== "running") throw new Error(`ticket ${pending.ticketId} has no running Worker`);
        store.pauseTicket(pending.ticketId);
        store.recordCommandReceipt(pending.trigger.id, pending.ticketId, "pause", true);
        enqueueReaction(store, pending.trigger, "+1");
        queueStatusCard(store, pending.ticketId);
        workers.deliver(
          assignment.id,
          `pause-${pending.trigger.id}-${assignment.id}`,
          "Pause this ticket after the current work finishes. Push your work in progress, then stop and wait for Ostrom to resume you.",
          () => {},
        );
      } else if (command === "resume") {
        const assignment = store.latestAssignment(pending.ticketId);
        if (assignment?.state !== "running") throw new Error(`ticket ${pending.ticketId} has no running Worker`);
        store.resumeTicket(pending.ticketId);
        store.recordCommandReceipt(pending.trigger.id, pending.ticketId, "resume", true);
        enqueueReaction(store, pending.trigger, "+1");
        queueStatusCard(store, pending.ticketId);
        workers.deliver(assignment.id, `resume-${pending.trigger.id}-${assignment.id}`, "This ticket has resumed. Continue your work.", () => {});
      } else {
        const assignment = store.latestAssignment(pending.ticketId);
        if (assignment?.state !== "running") throw new Error(`ticket ${pending.ticketId} has no running Worker`);
        store.recordCommandReceipt(pending.trigger.id, pending.ticketId, "stop", true);
        enqueueReaction(store, pending.trigger, "+1");
        workers.deliver(
          assignment.id,
          `stop-${pending.trigger.id}-${assignment.id}`,
          "Stop this ticket gracefully. Push your work in progress and summarise precisely where you stopped.",
          () => {},
          async (result) => {
            for (const pr of await pullRequests(gh, repo, assignment.branch!)) await gh(["pr", "ready", "--undo", pr]);
            store.endAssignment(assignment.id, "stopped by command");
            store.endTicket(pending.ticketId);
            store.enqueueReply({ id: `stopped-${pending.ticketId}`, threadId: pending.trigger.threadId, body: result.text, blocking: true });
            queueStatusCard(store, pending.ticketId);
          },
        );
      }
      pass.handled.push({ triggerId: pending.trigger.id, ticketId: pending.ticketId, command, allowed });
    } catch (error) {
      pass.failed.push({ triggerId: pending.trigger.id, ticketId: pending.ticketId, error: message(error) });
    }
  }
  return pass;
}

async function mayControl(
  store: Store,
  gh: GhRunner,
  repo: string,
  trigger: Trigger,
  _ticketId: number,
  permissions: Map<string, Promise<boolean>>,
  authors: Map<number, Promise<string>>,
): Promise<boolean> {
  const thread = store.getThread(trigger.threadId);
  const author = authors.get(thread.id) ?? issueAuthor(gh, repo, thread.number);
  authors.set(thread.id, author);
  return trigger.actor.toLowerCase() === (await author).toLowerCase() || collaborator(gh, repo, trigger.actor, permissions);
}

async function collaborator(gh: GhRunner, repo: string, user: string, cache: Map<string, Promise<boolean>>): Promise<boolean> {
  const key = user.toLowerCase();
  const found = cache.get(key) ?? ghGet(gh, `repos/${repo}/collaborators/${encodeURIComponent(user)}/permission`).then((body) => {
    // `permission` is the legacy level, where triage reads as "read"; `role_name` is the real role.
    const { permission, role_name: role } = body as { permission?: unknown; role_name?: unknown };
    return role === "triage" || role === "write" || role === "maintain" || role === "admin" ||
      permission === "write" || permission === "admin";
  }).catch(() => false);
  cache.set(key, found);
  return found;
}

async function issueAuthor(gh: GhRunner, repo: string, number: number): Promise<string> {
  const issue = await ghGet(gh, `repos/${repo}/issues/${number}`) as { user?: { login?: unknown } };
  return typeof issue.user?.login === "string" ? issue.user.login : "";
}

async function pullRequests(gh: GhRunner, repo: string, branch: string): Promise<string[]> {
  const owner = repo.split("/")[0]!;
  const response = await ghGet(gh, `repos/${repo}/pulls?head=${encodeURIComponent(`${owner}:${branch}`)}&state=open&per_page=100`);
  if (!Array.isArray(response)) throw new Error("expected open pull requests");
  return response.map((value) => {
    const url = (value as { html_url?: unknown }).html_url;
    if (typeof url !== "string") throw new Error("open pull request has no URL");
    return url;
  });
}

function enqueueReaction(store: Store, trigger: Trigger, reaction: "eyes" | "+1" | "confused"): void {
  if (trigger.kind === "pull_request_review") return;
  store.enqueueReply({ id: `command-${reaction}-${trigger.id}`, threadId: trigger.threadId, body: "", kind: "reaction", target: trigger.url, reaction });
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
