// The deterministic stuck ladder: detect signals in the Watchdog, make at most two silent same-
// session recovery attempts, then ask the Worker for one validated public reply.

import { queueStatusCard } from "../github/cards.ts";
import type { Store, Stall } from "../store/store.ts";
import type { Workers } from "../worker/workers.ts";

export interface StuckReply {
  tryingTo: string;
  blockedBy: string;
  tried: string;
  choices: string;
  workSoFar: string;
}

const FIELDS = ["tryingTo", "blockedBy", "tried", "choices", "workSoFar"] as const;

function numberedChoices(choices: string): string {
  const lines = choices.split("\n").map((line) => line.trim()).filter(Boolean);
  if (lines.length > 0 && lines.every((line) => /^\d+\.\s+\S/.test(line))) return lines.join("\n");

  const alternatives = lines.length > 1 ? lines : choices.split(/\s+or\s+|;/i).map((choice) => choice.trim().replace(/,$/, "")).filter(Boolean);
  if (alternatives.length < 2) return "1. provide the missing decision\n2. stop this ticket";
  return alternatives.map((choice, index) => `${index + 1}. ${choice}`).join("\n");
}

export function renderStuckReply(reply: StuckReply): string {
  return [
    "**I'm stuck on this and need a decision.**",
    `**Trying to:** ${reply.tryingTo}`,
    `**Blocked by:** ${reply.blockedBy}`,
    `**Tried:** ${reply.tried}`,
    `**I need you to choose:** ${numberedChoices(reply.choices)}`,
    `**Work so far:** ${reply.workSoFar} Reply with a number, or anything else to redirect.`,
  ].join("\n");
}

export function parseStuckReply(text: string, cause: string): StuckReply {
  try {
    const value = JSON.parse(text) as Record<string, unknown>;
    if (FIELDS.every((field) => typeof value[field] === "string" && (value[field] as string).trim() !== "")) {
      return {
        tryingTo: value["tryingTo"] as string, blockedBy: value["blockedBy"] as string,
        tried: value["tried"] as string, choices: value["choices"] as string, workSoFar: value["workSoFar"] as string,
      };
    }
  } catch {
    // The fallback is deliberately complete and truthful even if the Worker did not return JSON.
  }
  return {
    tryingTo: "complete the requested work", blockedBy: cause,
    tried: "two automatic recovery attempts on the same Worker session",
    choices: "1. provide the missing decision\n2. stop this ticket",
    workSoFar: "The Worker was asked to push its work in progress before this message.",
  };
}

export async function advanceStuckLadder(store: Store, workers: Workers): Promise<{ recovered: number[]; published: number[] }> {
  const result = { recovered: [] as number[], published: [] as number[] };
  for (const stall of store.unansweredStalls()) {
    if (stall.recoveryAttempts < 2 && stall.publicAt === null) {
      const started = store.beginStallRecovery(stall.id);
      if (started === null) continue;
      workers.cancel(stall.assignmentId);
      workers.deliver(
        stall.assignmentId,
        `stuck-recovery-${stall.id}-${started.recoveryAttempts}`,
        started.recoveryAttempts === 1
          ? "Ostrom detected that this ticket is stuck. Check the state, make progress, and continue the work."
          : "Ostrom still sees this ticket as stuck. Retry the work from the current state and make progress.",
        () => {},
      );
      queueStatusCard(store, store.getAssignment(stall.assignmentId).ticketId);
      result.recovered.push(stall.id);
      continue;
    }
    if (stall.publicAt !== null) continue;
    await publish(store, workers, stall);
    result.published.push(stall.id);
  }
  return result;
}

async function publish(store: Store, workers: Workers, stall: Stall): Promise<void> {
  const assignment = store.getAssignment(stall.assignmentId);
  const context = store.ticketContext(assignment.ticketId);
  store.publishStall(stall.id);
  if (store.getTicket(assignment.ticketId).state === "open") store.pauseTicket(assignment.ticketId);
  workers.cancel(assignment.id);
  workers.deliver(
    assignment.id,
    `stuck-public-${stall.id}`,
    [
      "Prepare the one GitHub stuck reply. Return JSON only with exactly these non-empty string fields:",
      '"tryingTo", "blockedBy", "tried", "choices", "workSoFar".',
      `The detected cause is: ${stall.cause ?? stall.semonReason ?? "unknown"}.`,
      "Do not include markdown or a code fence.",
    ].join(" "),
    () => {},
    async (turn) => {
      const reply = renderStuckReply(parseStuckReply(turn.text, stall.cause ?? stall.semonReason ?? "unknown"));
      store.enqueueReply({ id: `stuck-${stall.id}`, threadId: context.thread.id, body: reply, blocking: true });
      queueStatusCard(store, assignment.ticketId);
    },
  );
}
