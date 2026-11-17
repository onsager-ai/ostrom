import type { DatabaseSync } from "node:sqlite";

export interface SelectionDecision {
  id: number;
  ticketId: number;
  kind: "model" | "effort";
  value: string | null;
  reason: string;
  source: "router" | "override";
  input: string;
  at: string;
}

/** Append-only choices: operator overrides take precedence until another override replaces them. */
export class TicketSelections {
  readonly db: DatabaseSync;
  readonly now: () => Date;
  constructor(db: DatabaseSync, now: () => Date) { this.db = db; this.now = now; }

  record(ticketId: number, kind: SelectionDecision["kind"], value: string | null, reason: string,
    source: SelectionDecision["source"], input: unknown): void {
    this.db.prepare(`INSERT INTO selection_decision (ticket_id, kind, value, reason, source, input, at)
      VALUES (?, ?, ?, ?, ?, ?, ?)`).run(ticketId, kind, value, reason, source, JSON.stringify(input), this.now().toISOString());
  }

  history(ticketId: number): SelectionDecision[] {
    return this.db.prepare("SELECT * FROM selection_decision WHERE ticket_id = ? ORDER BY id").all(ticketId).map((row) => ({
      id: row["id"] as number, ticketId: row["ticket_id"] as number, kind: row["kind"] as SelectionDecision["kind"],
      value: row["value"] as string | null, reason: row["reason"] as string, source: row["source"] as SelectionDecision["source"],
      input: row["input"] as string, at: row["at"] as string,
    }));
  }

  current(ticketId: number, kind: SelectionDecision["kind"]): SelectionDecision | null {
    const history = this.history(ticketId).filter((row) => row.kind === kind);
    return history.findLast((row) => row.source === "override") ?? history.at(-1) ?? null;
  }
}
