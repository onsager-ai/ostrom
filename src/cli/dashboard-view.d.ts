// Types for the dashboard page's plain-JS helpers (served to the browser as-is, so no build step).

export interface AttentionItem { ticketId?: number; text: string }

export function attentionItems(
  status: {
    stuckTickets: { ticketId: number; cause: string | null; detectedAt: string }[];
    watchdog: { lastSuccess: boolean | null; error: string | null };
    machine?: { temperature: number | null } | null;
  },
  tickets: { id: number; state: string; pauseCause: string | null; budgetWarning: string | null }[],
): AttentionItem[];

export function ageLabel(timestamp: string | null | undefined, now?: number): string;

export function githubUrl(value: string): string | null;

export function createRefresh<T>(
  fetcher: () => Promise<T>,
  onSuccess: (snapshot: T, at: Date) => void,
  onFailure: (lastSnapshot: T | undefined, error: Error) => void,
): () => Promise<void>;
