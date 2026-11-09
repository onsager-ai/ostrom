// The Watchdog (docs/direction.md, milestone 1 step 6): a deterministic check over native
// Workers. It records each Semon stall episode once and leaves every judgement to the Router,
// which sees the episode in its next batch (AGENTS.md rule 3). It never ends work.
//
// Semon's `stalls` read is the one Semon call Ostrom makes:
//   semon query stalls --idle-minutes <N> --since 1d --json
// Semon lists every native session on this machine whose log has had no line for N minutes and
// that is waiting on a question, whose recorded-busy process is gone, or that is working with no
// output. A Worker's session is matched to its assignment by its git branch
// (`ostrom/ticket-<ticket>-<assignment>`). A failed read is reported and never clears an episode.

import { budgetReached } from "../budget/budgets.ts";
import type { ProcessRunner } from "../process.ts";
import type { Store } from "../store/store.ts";

/** The window of session logs Semon reads. */
export const SEMON_WINDOW = "1d";
/** How much of a Worker's open question the stall record keeps. */
export const DETAIL_CHARS = 300;

export interface SemonStall {
  id: string;
  /** The session's git branch; null outside a repository. */
  branch: string | null;
  /** Why Semon calls it stalled: process-gone, waiting-question or no-output at the time of writing. */
  stallReason: string;
  openQuestion: string | null;
}

export class SemonOutputError extends Error {
  override readonly name = "SemonOutputError";
}

export function semonStallsArgs(idleMinutes: number): string[] {
  return ["query", "stalls", "--idle-minutes", String(idleMinutes), "--since", SEMON_WINDOW, "--json"];
}

/** Runs Semon's `stalls` read. A non-zero exit or output that does not parse is an error. */
export async function readSemonStalls(semon: ProcessRunner, idleMinutes: number): Promise<SemonStall[]> {
  const args = semonStallsArgs(idleMinutes);
  const exit = await semon({ args });
  if (exit.code !== 0) {
    const detail = exit.stderr.trim() || exit.stdout.trim() || "no output";
    throw new Error(`semon query stalls exited ${exit.code ?? "by signal"}: ${detail}`);
  }
  return parseSemonStalls(exit.stdout, idleMinutes);
}

/**
 * Parses `semon query stalls --json`, narrowed to the fields the Watchdog reads. Extra fields are
 * ignored; a missing or mistyped field read here throws, as does an answer for another threshold.
 */
export function parseSemonStalls(stdout: string, idleMinutes: number): SemonStall[] {
  let value: unknown;
  try {
    value = JSON.parse(stdout);
  } catch {
    throw new SemonOutputError("semon printed output that is not JSON");
  }
  const root = record(value, "$");
  if (root["idle_minutes"] !== idleMinutes) {
    throw new SemonOutputError(`$.idle_minutes: expected ${idleMinutes}, got ${JSON.stringify(root["idle_minutes"])}`);
  }
  const stalls: unknown = root["stalls"];
  if (!Array.isArray(stalls)) throw new SemonOutputError("$.stalls: expected an array");
  return stalls.map((item: unknown, index: number): SemonStall => {
    const path = `$.stalls[${index}]`;
    const stall = record(item, path);
    const question = stall["open_question"];
    return {
      id: string(stall["id"], `${path}.id`),
      branch: stall["branch"] === null ? null : string(stall["branch"], `${path}.branch`),
      stallReason: string(stall["stall_reason"], `${path}.stall_reason`),
      openQuestion:
        question === null ? null : string(record(question, `${path}.open_question`)["text"], `${path}.open_question.text`),
    };
  });
}

export interface WatchdogOptions {
  store: Store;
  semon: ProcessRunner;
  idleMinutes: number;
  /** Clock used for budget evaluation; injectable for deterministic checks. */
  now?: () => Date;
}

export interface Checked {
  assignmentId: number;
  semonReason: string | null;
  /** The open stall episode, or null when the Worker is not stalled. */
  stallId: number | null;
  /** This pass opened the episode: it is flagged to the Router's next batch. */
  newEpisode: boolean;
  /** The episode this pass cleared, if any. */
  clearedStallId: number | null;
}

export interface WatchdogPass {
  checked: Checked[];
  /** Reads this pass could not do; the command exits non-zero when any is listed. */
  failed: { what: string; error: string }[];
}

/** One pass over running assignments. */
export async function watchdog(options: WatchdogOptions): Promise<WatchdogPass> {
  const { store, idleMinutes, now = () => new Date() } = options;
  const pass: WatchdogPass = { checked: [], failed: [] };
  let semon: SemonStall[] | null;
  try {
    semon = await readSemonStalls(options.semon, idleMinutes);
  } catch (error) {
    semon = null;
    pass.failed.push({ what: "semon stalls", error: message(error) });
  }
  for (const assignment of store.runningAssignments()) {
    const matched = assignment.branch === null ? undefined : semon?.find((stall) => stall.branch === assignment.branch);
    const semonReason = matched?.stallReason ?? null;
    const checked: Checked = {
      assignmentId: assignment.id,
      semonReason,
      stallId: null,
      newEpisode: false,
      clearedStallId: null,
    };
    const budget = budgetReached(store.ticketBudget(assignment.ticketId), now());
    const repeatedError = assignment.consecutiveErrors >= 2;
    if (semonReason !== null || budget !== null || repeatedError) {
      const question = matched?.openQuestion ?? null;
      const detail =
        question === null ? assignment.lastError : question.length > DETAIL_CHARS ? `${question.slice(0, DETAIL_CHARS)}…` : question;
      const cause = budget === null ? repeatedError ? "same turn error twice" : semonReason! : `budget reached: ${budget}`;
      const recorded = store.recordStall({ assignmentId: assignment.id, semonReason, detail, cause });
      checked.stallId = recorded.id;
      checked.newEpisode = !recorded.duplicate;
    } else if (semon !== null) {
      checked.clearedStallId = store.clearStall(assignment.id);
    }
    pass.checked.push(checked);
  }
  return pass;
}

function record(value: unknown, path: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new SemonOutputError(`${path}: expected an object`);
  }
  return value as Record<string, unknown>;
}

function string(value: unknown, path: string): string {
  if (typeof value !== "string") throw new SemonOutputError(`${path}: expected a string, got ${JSON.stringify(value)}`);
  return value;
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
