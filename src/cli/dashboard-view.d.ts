// Types for the dashboard page's plain-JS helpers (served to the browser as-is, so no build step).
import type { OperatorTicket, OperatorTicketDetail } from "../store/store.ts";

export interface AttentionItem { ticketId?: number; text: string }
export interface TicketPhase {
  key: "finished" | "paused" | "failed" | "working" | "approval" | "quiet" | "ci" | "revising" | "blocked" | "ready";
  label: string;
  tone: "neutral" | "amber" | "red" | "blue" | "green";
  detail: string;
}
export interface BudgetMeter {
  label: string;
  used: number;
  limit: number;
  percent: number | null;
  warning: boolean;
  text: string;
}
export function ticketPhase(ticket: Pick<OperatorTicket, "state" | "pauseCause" | "assignment" | "latestFailure" | "budget" | "pullRequests" | "mergeCandidate">): TicketPhase;
export function phaseSummary(tickets: OperatorTicket[]): string;
export function ticketSummary(detail?: Pick<OperatorTicketDetail, "decisions" | "context">): { full: string; short: string };
export function pendingSelection(
  detail: {
    selections: Pick<OperatorTicketDetail["selections"][number], "id" | "kind" | "source" | "value">[];
    assignment?: Pick<NonNullable<OperatorTicket["assignment"]>, "harness" | "model"> | null;
  } | null | undefined,
  kind: "model" | "effort",
  current: string | null | undefined,
  boundary?: string | null,
): string | null;
export function budgetMeters(ticket: Pick<OperatorTicket, "budgetUsage" | "budget">): BudgetMeter[];
export function attentionItems(
  status: {
    stuckTickets: { ticketId: number; cause: string | null; detectedAt: string }[];
    watchdog: { lastSuccess: boolean | null; error: string | null };
    machine?: { temperature: number | null } | null;
  },
  tickets: (Pick<OperatorTicket, "id" | "state" | "pauseCause" | "budgetWarning"> & Partial<Pick<OperatorTicket, "assignment" | "latestFailure">>)[],
): AttentionItem[];
export function policyLabel(policy: string): string;
export function localTime(timestamp: string | null | undefined): string;
export function ageLabel(timestamp: string | null | undefined, now?: number): string;
export function githubUrl(value: string): string | null;
export function createRefresh<T>(
  fetcher: () => Promise<T>,
  onSuccess: (snapshot: T, at: Date) => void,
  onFailure: (lastSnapshot: T | undefined, error: Error) => void,
): () => Promise<void>;
