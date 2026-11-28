import { execFile } from "node:child_process";
import { promisify } from "node:util";

import type { PlatformStore } from "./store.ts";
import { budgetPeriods, isSemonKey, spendWindow } from "./types.ts";
import type { SemonSessionIdentity, SpendRollup, SpendSnapshot } from "./types.ts";

export type SpendLookup =
  | { status: "available"; snapshot: Readonly<SpendSnapshot> }
  | { status: "not_found" | "invalid_session" | "unavailable"; snapshot: null };

export interface SpendSource {
  getSnapshot(sessionUid: string): Promise<SpendLookup>;
}

export function unavailableSpendSource(): SpendSource {
  return { getSnapshot: async () => ({ status: "unavailable", snapshot: null }) };
}

type SemonRunner = (bin: string, args: string[]) => Promise<string>;

const exec = promisify(execFile);
const runSemon: SemonRunner = async (bin, args) => {
  const { stdout } = await exec(bin, args, { timeout: 5_000, maxBuffer: 1024 * 1024 });
  return stdout;
};

const factFields = [
  "input_tokens", "output_tokens", "cache_read_tokens", "cache_write_5m_tokens", "cache_write_1h_tokens",
  "usd_est", "reported_usd", "usage_complete",
] as const;
const identityFields = ["origin_namespace", "native_harness", "native_session_id"];
const invalidSession = { status: "invalid_session", snapshot: null } as const;
const unavailable = { status: "unavailable", snapshot: null } as const;

interface SemonQuery {
  rows: unknown[][];
  snapshot?: unknown;
  snapshot_id?: unknown;
  as_of?: unknown;
  coverage?: unknown;
  truncated: boolean;
}

async function readSemonSchema(bin: string, run: SemonRunner): Promise<Map<string, Set<string>>> {
  const result = JSON.parse(await run(bin, ["query", "schema"])) as {
    tables: { name: string; columns: { name: string }[] }[];
  };
  return new Map(result.tables.map(({ name, columns }) => [name, new Set(columns.map((column) => column.name))]));
}

function requireSemonFields(schema: Map<string, Set<string>>, table: string, required: readonly string[]): Set<string> {
  const fields = schema.get(table);
  if (fields === undefined || !required.every((column) => fields.has(column))) throw new Error("unsupported Semon schema");
  return fields;
}

async function readSemonQuery(bin: string, run: SemonRunner, sql: string, params: string[] = []): Promise<SemonQuery> {
  const result = JSON.parse(await run(bin, ["query", "sql", sql, ...params])) as SemonQuery;
  if (result.truncated !== false || !Array.isArray(result.rows) || result.rows.length > 2
    || !result.rows.every(Array.isArray)) throw new Error("invalid Semon query result");
  return result;
}

const rollupSql = `SELECT
  count(DISTINCT session_uid) AS session_count,
  count(*) FILTER (WHERE usage_owner = 'exclusive') AS exclusive_steps,
  count(*) FILTER (WHERE usage_owner = 'exclusive' AND price_status = 'priced') AS priced_steps,
  count(*) FILTER (WHERE usage_owner = 'exclusive' AND price_status = 'unpriced') AS unpriced_steps,
  count(*) FILTER (WHERE usage_owner <> 'exclusive') AS non_exclusive_steps,
  sum(usd_est) FILTER (WHERE usage_owner = 'exclusive' AND price_status = 'priced') AS priced_usd_est
FROM fact_step
WHERE epoch_ms(ts) >= ? AND epoch_ms(ts) < ?`;

export async function querySemonTeamRollups(bin: string, now: Date | string, run: SemonRunner = runSemon): Promise<SpendRollup[]> {
  requireSemonFields(await readSemonSchema(bin, run), "fact_step", [
    "session_uid", "ts", "usage_owner", "price_status", "usd_est", "price_version",
  ]);
  const rollups: SpendRollup[] = [];
  for (const period of budgetPeriods) {
    const { since, until } = spendWindow(period, now);
    const params = JSON.stringify([Date.parse(since), Date.parse(until)]);
    const result = await readSemonQuery(bin, run, rollupSql, ["--params", params]);
    const row = result.rows[0];
    if (result.rows.length !== 1 || row?.length !== 6) throw new Error("invalid Semon spend rollup result");
    const sessionCount = rollupCount(row[0]);
    const unpricedCount = rollupCount(row[3]);
    const incompleteCount = rollupCount(unpricedCount + rollupCount(row[4]));
    const usdEst = money(row[5], 0);
    rollups.push({
      scopeKind: "team", scopeId: null, period, source: "semon", since, until, sessionCount, unpricedCount, incompleteCount,
      usdEst, sourceReportedUsd: null, billedUsd: null, preferredUsd: usdEst,
      // One origin's priced exclusive steps cannot establish complete team spend.
      coverage: "partial", snapshotId: metadata(result.snapshot_id ?? result.snapshot),
      asOf: metadata(result.as_of), observedAt: until,
    });
  }
  return rollups;
}

export class SemonCliSpendSource implements SpendSource {
  private readonly bin: string;
  private readonly run: SemonRunner;
  private schema: Map<string, Set<string>> | undefined;

  constructor(bin: string, run: SemonRunner = runSemon) {
    this.bin = bin;
    this.run = run;
  }

  private async fields(table: string, required: readonly string[]): Promise<Set<string>> {
    this.schema ??= await readSemonSchema(this.bin, this.run);
    return requireSemonFields(this.schema, table, required);
  }

  private async query(sql: string): Promise<SemonQuery> {
    return readSemonQuery(this.bin, this.run, sql);
  }

  async supportsSessionIdentity(): Promise<boolean> {
    try {
      await this.fields("dim_session", ["session_uid", ...identityFields]);
      return true;
    } catch {
      return false;
    }
  }

  async resolveSessionUid({ originNamespace, nativeHarness, nativeSessionId }: SemonSessionIdentity): Promise<string | null> {
    const values = [originNamespace, nativeHarness, nativeSessionId];
    if (!values.every(isSemonKey)) return null;
    try {
      await this.fields("dim_session", ["session_uid", ...identityFields]);
      const where = identityFields.map((field, index) => `${field} = '${values[index]}'`).join(" AND ");
      const { rows } = await this.query(`SELECT session_uid FROM dim_session WHERE ${where} LIMIT 2`);
      const uid = rows[0]?.[0];
      return rows.length === 1 && typeof uid === "string" && isSemonKey(uid) ? uid : null;
    } catch {
      return null;
    }
  }

  async getSnapshot(sessionUid: string): Promise<SpendLookup> {
    if (!isSemonKey(sessionUid)) return invalidSession;
    let result: SemonQuery;
    try {
      const fields = await this.fields("fact_session", ["session_uid", ...factFields]);
      const billed = fields.has("billed_usd") ? "billed_usd" : "NULL";
      const coverage = fields.has("coverage") ? "coverage"
        : fields.has("source_status") && fields.has("computation_status")
          ? "CASE WHEN source_status <> 'complete' OR computation_status <> 'current' THEN 'partial' END"
          : "NULL";
      const asOf = fields.has("as_of") ? "as_of" : "NULL";
      result = await this.query(`SELECT ${factFields.join(", ")}, ${billed}, ${coverage}, ${asOf}
        FROM fact_session WHERE session_uid = '${sessionUid}' LIMIT 2`);
    } catch {
      return unavailable;
    }
    try {
      const row = result.rows[0];
      if (row === undefined) return { status: "not_found", snapshot: null };
      if (result.rows.length !== 1 || row.length !== 11 || typeof row[7] !== "boolean") return invalidSession;
      const [inputTokens, outputTokens, read, write5m, write1h] = row.slice(0, 5).map(tokenCount);
      const cached = [read, write5m, write1h];
      const cachedTokens = cached.some((count) => count == null) ? null
        : cached.reduce<number>((total, count) => total + (count ?? 0), 0);
      if (cachedTokens !== null && !Number.isSafeInteger(cachedTokens)) return invalidSession;
      return { status: "available", snapshot: {
        sessionUid, source: "semon", inputTokens: inputTokens ?? null, outputTokens: outputTokens ?? null, cachedTokens,
        usdEst: money(row[5]), sourceReportedUsd: money(row[6]), billedUsd: money(row[8]),
        coverage: metadata(row[9] ?? result.coverage) ?? (row[7] ? "complete" : "partial"),
        asOf: metadata(row[10] ?? result.as_of), snapshotId: metadata(result.snapshot_id ?? result.snapshot), observedAt: new Date().toISOString(),
      } };
    } catch {
      return invalidSession;
    }
  }
}

function tokenCount(value: unknown): number | null {
  if (value === null) return null;
  if (typeof value === "number" && Number.isSafeInteger(value) && value >= 0) return value;
  throw new Error("invalid Semon token count");
}

function rollupCount(value: unknown): number {
  if (typeof value === "number" && Number.isSafeInteger(value) && value >= 0) return value;
  throw new Error("invalid Semon spend rollup count");
}

function money(value: unknown, min = -Infinity): number | null {
  if (value === null) return null;
  if (typeof value === "number" && Number.isFinite(value) && value >= min) return value;
  throw new Error("invalid Semon cost");
}

function metadata(value: unknown): string | null {
  if (value == null) return null;
  return typeof value === "string" ? value : JSON.stringify(value);
}

export async function refreshSpendSnapshots(
  store: PlatformStore, source: SpendSource, signal: AbortSignal,
): Promise<void> {
  const runs = store.listRuns();
  const unlinked = runs.filter((run) => run.semonSessionUid === null && run.semonIdentity !== null);
  const uids = new Set(runs.flatMap((run) => run.semonSessionUid === null ? [] : [run.semonSessionUid]));
  if (unlinked.length > 0 && source instanceof SemonCliSpendSource && await source.supportsSessionIdentity()) {
    for (const run of unlinked) {
      if (signal.aborted) return;
      if (run.semonIdentity === null) continue;
      const uid = await source.resolveSessionUid(run.semonIdentity);
      if (signal.aborted) return;
      if (uid !== null && store.linkRunSemonSession(run.id, uid, run.semonIdentity) !== null) uids.add(uid);
    }
  }
  const sessions = [...uids];
  for (const [index, uid] of sessions.entries()) {
    if (signal.aborted) return;
    const lookup = await source.getSnapshot(uid);
    if (signal.aborted) return;
    if (lookup.status === "unavailable") {
      store.markSemonRefreshFailed(sessions.slice(index));
      return;
    }
    if (lookup.status === "available") store.upsertSpendSnapshot(lookup.snapshot);
    else store.markSemonRefreshFailed([uid]);
  }
}
