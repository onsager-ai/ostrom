import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import type { DoctorContext } from "../lib/context.js";
import type { CheckResult, Status } from "../lib/result.js";

const TRACE_STALE_SECONDS = 24 * 60 * 60;
const MAX_DATE_EPOCH_SECONDS = 8_640_000_000_000;

interface Health {
  status: Status;
  detail: string;
  remedy: string;
}

interface LeaseRecord {
  owner: string;
  started_at: number;
  expires_at: number;
}

function nowEpoch(context: DoctorContext): number {
  const explicit = context.env.MANDATE_NOW_EPOCH;
  if (explicit && /^\d+$/.test(explicit)) return Number(explicit);

  const sweepTime = context.env.MANDATE_SWEEP_TIME;
  if (sweepTime) {
    const parsed = Date.parse(sweepTime);
    if (Number.isFinite(parsed)) return Math.floor(parsed / 1000);
  }
  return Math.floor(Date.now() / 1000);
}

function exactKeys(value: object, expected: string[]): boolean {
  return JSON.stringify(Object.keys(value).sort()) === JSON.stringify(expected);
}

function jsonObject(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function traceHealth(path: string, now: number): Health {
  if (!existsSync(path)) {
    return {
      status: "WARN",
      detail: "trace absent",
      remedy: "run /ostrom:gatekeep and confirm it creates sprint.jsonl",
    };
  }

  let source: string;
  try {
    source = readFileSync(path, "utf8");
  } catch {
    return {
      status: "WARN",
      detail: "trace unreadable",
      remedy: "inspect sprint.jsonl and fix its permissions",
    };
  }

  const lines = source.split(/\r?\n/).filter((line) => line.length > 0);
  if (lines.length === 0) {
    return {
      status: "WARN",
      detail: "trace present but empty",
      remedy: "run /ostrom:gatekeep and confirm it appends a complete pass",
    };
  }

  let record: unknown;
  try {
    record = JSON.parse(lines.at(-1) ?? "");
  } catch {
    return {
      status: "WARN",
      detail: "trace last record is unreadable",
      remedy: "inspect sprint.jsonl and repair or remove its malformed last record",
    };
  }
  if (
    !jsonObject(record) ||
    !exactKeys(record, ["fact", "kind", "narration", "ts"]) ||
    typeof record.ts !== "string" ||
    record.ts.length === 0 ||
    typeof record.kind !== "string" ||
    record.kind.length === 0 ||
    !jsonObject(record.fact) ||
    !jsonObject(record.narration)
  ) {
    return {
      status: "WARN",
      detail: "trace last record has an invalid shape",
      remedy: "inspect sprint.jsonl; records must be written by trace.sh append",
    };
  }

  const timestamp = record.ts;
  const timestampMs = Date.parse(timestamp);
  if (!timestamp || !Number.isFinite(timestampMs)) {
    return {
      status: "WARN",
      detail: "trace last record has an invalid timestamp",
      remedy: "inspect sprint.jsonl; records must be written by trace.sh append",
    };
  }

  const ageSeconds = now - Math.floor(timestampMs / 1000);
  if (ageSeconds > TRACE_STALE_SECONDS) {
    return {
      status: "WARN",
      detail: `trace stale, last ${timestamp} (older than 24h)`,
      remedy: "run /ostrom:gatekeep and confirm the recurring loop is active",
    };
  }
  return {
    status: "OK",
    detail: `trace current, last ${timestamp}`,
    remedy: "",
  };
}

function validLease(value: unknown): value is LeaseRecord {
  if (!value || typeof value !== "object") return false;
  if (!exactKeys(value, ["expires_at", "owner", "started_at"])) return false;
  const lease = value as Partial<LeaseRecord>;
  return (
    typeof lease.owner === "string" &&
    lease.owner.length > 0 &&
    Number.isSafeInteger(lease.started_at) &&
    (lease.started_at ?? -1) >= 0 &&
    (lease.started_at ?? Infinity) <= MAX_DATE_EPOCH_SECONDS &&
    Number.isSafeInteger(lease.expires_at) &&
    (lease.expires_at ?? -1) >= (lease.started_at ?? 0) &&
    (lease.expires_at ?? Infinity) <= MAX_DATE_EPOCH_SECONDS
  );
}

function leaseHealth(path: string, now: number): Health {
  if (!existsSync(path)) {
    return { status: "OK", detail: "lease idle", remedy: "" };
  }

  let source: string;
  try {
    source = readFileSync(path, "utf8");
  } catch {
    return {
      status: "WARN",
      detail: "lease unreadable",
      remedy: "inspect sprint.lease and fix its permissions",
    };
  }

  let lease: unknown;
  try {
    lease = JSON.parse(source);
  } catch {
    return {
      status: "WARN",
      detail: "lease unreadable",
      remedy: "inspect sprint.lease; only lease.sh may create or remove it",
    };
  }
  if (!validLease(lease)) {
    return {
      status: "WARN",
      detail: "lease has an invalid shape",
      remedy: "inspect sprint.lease; only lease.sh may create or remove it",
    };
  }

  const expiry = new Date(lease.expires_at * 1000).toISOString();
  if (now >= lease.expires_at) {
    return {
      status: "WARN",
      detail: `lease stale for ${lease.owner}, expired ${expiry}`,
      remedy: "allow the next gatekeeper pass to reclaim the expired lease",
    };
  }
  return {
    status: "OK",
    detail: `lease held by ${lease.owner} until ${expiry}`,
    remedy: "",
  };
}

export function checkTraceLease(context: DoctorContext): CheckResult {
  const dataDir = join(context.configDir, "ostrom");
  const now = nowEpoch(context);
  const trace = traceHealth(join(dataDir, "sprint.jsonl"), now);
  const lease = leaseHealth(join(dataDir, "sprint.lease"), now);
  const warned = trace.status === "WARN" || lease.status === "WARN";

  return {
    status: warned ? "WARN" : "OK",
    name: "trace-lease",
    detail: `${trace.detail}; ${lease.detail}`,
    remedy: [trace.remedy, lease.remedy].filter(Boolean).join("; "),
  };
}
