import { execFile } from "node:child_process";
import { promisify } from "node:util";

import type { PlatformStore } from "./store.ts";
import type { SpendSnapshot } from "./types.ts";

export type SpendLookup =
  | { status: "available"; snapshot: Readonly<SpendSnapshot> }
  | { status: "not_found" | "unavailable"; snapshot: null };

export interface SpendSource {
  getSnapshot(sessionUid: string): Promise<SpendLookup>;
}

export function unavailableSpendSource(): SpendSource {
  return { getSnapshot: async () => ({ status: "unavailable", snapshot: null }) };
}

type SemonRunner = (bin: string, args: string[]) => Promise<string>;
type SessionIdentity = { originNamespace: string; nativeHarness: string; nativeSessionId: string };

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
const safeKey = (key: string) => /^[\w.:@/+=-]{1,256}$/.test(key);
const unavailable = { status: "unavailable", snapshot: null } as const;

interface SemonQuery {
  rows: unknown[][];
  snapshot?: unknown;
  snapshot_id?: unknown;
  as_of?: unknown;
  coverage?: unknown;
  truncated: boolean;
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
    if (this.schema === undefined) {
      const result = JSON.parse(await this.run(this.bin, ["query", "schema"])) as {
        tables: { name: string; columns: { name: string }[] }[];
      };
      this.schema = new Map(result.tables.map(({ name, columns }) => [name, new Set(columns.map((column) => column.name))]));
    }
    const fields = this.schema.get(table);
    if (fields === undefined || !required.every((column) => fields.has(column))) throw new Error("unsupported Semon schema");
    return fields;
  }

  private async query(sql: string): Promise<SemonQuery> {
    const result = JSON.parse(await this.run(this.bin, ["query", "sql", sql])) as SemonQuery;
    if (result.truncated !== false || !Array.isArray(result.rows) || result.rows.length > 2
      || !result.rows.every(Array.isArray)) throw new Error("invalid Semon query result");
    return result;
  }

  async resolveSessionUid({ originNamespace, nativeHarness, nativeSessionId }: SessionIdentity): Promise<string | null> {
    const values = [originNamespace, nativeHarness, nativeSessionId];
    if (!values.every(safeKey)) return null;
    try {
      await this.fields("dim_session", ["session_uid", ...identityFields]);
      const where = identityFields.map((field, index) => `${field} = '${values[index]}'`).join(" AND ");
      const { rows } = await this.query(`SELECT session_uid FROM dim_session WHERE ${where} LIMIT 2`);
      const uid = rows[0]?.[0];
      return rows.length === 1 && typeof uid === "string" && safeKey(uid) ? uid : null;
    } catch {
      return null;
    }
  }

  async getSnapshot(sessionUid: string): Promise<SpendLookup> {
    if (!safeKey(sessionUid)) return unavailable;
    try {
      const fields = await this.fields("fact_session", ["session_uid", ...factFields]);
      const billed = fields.has("billed_usd") ? "billed_usd" : "NULL";
      const coverage = fields.has("coverage") ? "coverage"
        : fields.has("source_status") && fields.has("computation_status")
          ? "CASE WHEN source_status <> 'complete' OR computation_status <> 'current' THEN 'partial' END"
          : "NULL";
      const asOf = fields.has("as_of") ? "as_of" : "NULL";
      const result = await this.query(`SELECT ${factFields.join(", ")}, ${billed}, ${coverage}, ${asOf}
        FROM fact_session WHERE session_uid = '${sessionUid}' LIMIT 2`);
      const row = result.rows[0];
      if (row === undefined) return { status: "not_found", snapshot: null };
      if (result.rows.length !== 1 || row.length !== 11 || typeof row[7] !== "boolean") return unavailable;
      const [inputTokens, outputTokens, read, write5m, write1h] = row.slice(0, 5).map(tokenCount);
      const cached = [read, write5m, write1h];
      const cachedTokens = cached.some((count) => count == null) ? null
        : cached.reduce<number>((total, count) => total + (count ?? 0), 0);
      if (cachedTokens !== null && !Number.isSafeInteger(cachedTokens)) return unavailable;
      return { status: "available", snapshot: {
        sessionUid, source: "semon", inputTokens: inputTokens ?? null, outputTokens: outputTokens ?? null, cachedTokens,
        usdEst: money(row[5]), sourceReportedUsd: money(row[6]), billedUsd: money(row[8]),
        coverage: metadata(row[9] ?? result.coverage) ?? (row[7] ? "complete" : "partial"),
        asOf: metadata(row[10] ?? result.as_of), snapshotId: metadata(result.snapshot_id ?? result.snapshot), observedAt: new Date().toISOString(),
      } };
    } catch {
      return unavailable;
    }
  }
}

function tokenCount(value: unknown): number | null {
  if (value === null) return null;
  if (typeof value === "number" && Number.isSafeInteger(value) && value >= 0) return value;
  throw new Error("invalid Semon token count");
}

function money(value: unknown): number | null {
  if (value === null) return null;
  if (typeof value === "number" && Number.isFinite(value)) return value;
  throw new Error("invalid Semon cost");
}

function metadata(value: unknown): string | null {
  if (value == null) return null;
  return typeof value === "string" ? value : JSON.stringify(value);
}

export async function refreshSpendSnapshots(store: PlatformStore, source: SpendSource, signal: AbortSignal): Promise<void> {
  const sessionUids = new Set(store.listRuns().flatMap((run) => run.semonSessionUid === null ? [] : [run.semonSessionUid]));
  for (const uid of sessionUids) {
    if (signal.aborted) return;
    const lookup = await source.getSnapshot(uid);
    if (signal.aborted || lookup.status === "unavailable") return;
    if (lookup.status === "available") store.upsertSpendSnapshot(lookup.snapshot);
  }
}
