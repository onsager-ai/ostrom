import type { DatabaseSync } from "node:sqlite";
import type { Harness } from "../session/models.ts";

export interface ModelOutcome {
  repository: string;
  kind: string;
  harness: Harness;
  model: string;
  started: number;
  merged: number;
  medianTurns: number;
  medianFreshTokens: number;
  medianReviewRounds: number;
}

function median(values: number[]): number {
  values.sort((a, b) => a - b);
  const middle = Math.floor(values.length / 2);
  return values.length % 2 ? values[middle]! : (values[middle - 1]! + values[middle]!) / 2;
}

/** One sample per ticket/model, including failed and still-running work. No inferred legacy kinds. */
export function modelOutcomes(db: DatabaseSync, repository?: string): ModelOutcome[] {
  const rows = db.prepare(`
    SELECT source.name AS repository, first.kind, turn.harness, turn.requested_model AS model,
      turn.ticket_id, COUNT(*) AS turns,
      SUM(MAX(0, turn.input_tokens - turn.cached_input_tokens) + turn.output_tokens) AS fresh_tokens,
      SUM(CASE WHEN turn.kind = 'review' THEN 1 ELSE 0 END) AS reviews,
      EXISTS (
        SELECT 1 FROM pr_merge AS merged
        WHERE merged.repository = source.name AND (
          (thread.kind = 'pull_request' AND thread.number = merged.pr_number)
          OR EXISTS (
            SELECT 1 FROM merge_candidate AS candidate
            JOIN assignment ON assignment.id = candidate.assignment_id
            WHERE assignment.ticket_id = ticket.id AND candidate.repository = merged.repository
              AND candidate.pr_number = merged.pr_number
          )
          OR EXISTS (
            SELECT 1 FROM subscription JOIN thread AS pr ON pr.id = subscription.thread_id
            WHERE subscription.ticket_id = ticket.id AND pr.kind = 'pull_request'
              AND pr.source_id = source.id AND pr.number = merged.pr_number
          )
        )
      ) AS merged
    FROM turn
    JOIN ticket ON ticket.id = turn.ticket_id
    JOIN thread ON thread.id = ticket.thread_id
    JOIN source ON source.id = thread.source_id
    JOIN turn AS first ON first.id = (SELECT MIN(id) FROM turn WHERE ticket_id = ticket.id)
    WHERE (? IS NULL OR source.name = ?)
    GROUP BY source.name, first.kind, turn.harness, turn.requested_model, ticket.id
    ORDER BY source.name, first.kind, turn.harness, turn.requested_model, ticket.id
  `).all(repository ?? null, repository ?? null);
  const groups = new Map<string, { outcome: ModelOutcome; turns: number[]; tokens: number[]; reviews: number[] }>();
  for (const row of rows) {
    const repository = row["repository"] as string;
    const kind = row["kind"] as string;
    const harness = row["harness"] as Harness;
    const model = row["model"] as string;
    const key = JSON.stringify([repository, kind, harness, model]);
    let group = groups.get(key);
    if (!group) {
      group = { outcome: { repository, kind, harness, model, started: 0, merged: 0, medianTurns: 0, medianFreshTokens: 0, medianReviewRounds: 0 }, turns: [], tokens: [], reviews: [] };
      groups.set(key, group);
    }
    group.outcome.started++;
    group.outcome.merged += Number(row["merged"]);
    group.turns.push(Number(row["turns"]));
    group.tokens.push(Number(row["fresh_tokens"]));
    group.reviews.push(Number(row["reviews"]));
  }
  return [...groups.values()].map(({ outcome, turns, tokens, reviews }) => ({
    ...outcome, medianTurns: median(turns), medianFreshTokens: median(tokens), medianReviewRounds: median(reviews),
  }));
}
