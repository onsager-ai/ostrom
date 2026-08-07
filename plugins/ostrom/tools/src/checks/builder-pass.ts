import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import type { DoctorContext } from "../lib/context.js";
import { resolveMandateCadenceHours } from "../lib/config.js";
import type { CheckResult } from "../lib/result.js";

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

function object(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function lastBuilderPassEnded(source: string): Record<string, unknown> | undefined {
  let contentEnd = source.length;
  while (contentEnd > 0) {
    while (
      contentEnd > 0 &&
      (source[contentEnd - 1] === "\n" || source[contentEnd - 1] === "\r")
    ) {
      contentEnd -= 1;
    }
    if (contentEnd === 0) break;

    const lineStart = source.lastIndexOf("\n", contentEnd - 1) + 1;
    const line = source.slice(lineStart, contentEnd);
    contentEnd = lineStart > 0 ? lineStart - 1 : 0;

    let record: unknown;
    try {
      record = JSON.parse(line);
    } catch {
      continue;
    }
    if (!object(record) || record.kind !== "pass-ended" || !object(record.fact)) {
      continue;
    }
    const owner = record.fact.owner;
    if (typeof owner === "string" && owner.startsWith("builder-")) return record;
  }
  return undefined;
}

export function checkBuilderPass(context: DoctorContext): CheckResult {
  const cadenceHours = resolveMandateCadenceHours(
    context.pluginRoot,
    context.configDir,
    context.cwd,
  );
  if (cadenceHours === undefined) {
    return {
      status: "WARN",
      name: "builder-pass",
      detail: "builder cadence is invalid",
      remedy: "set cadence_hours to a positive integer in mandates.yaml",
    };
  }

  const tracePath = join(context.configDir, "ostrom", "sprint.jsonl");
  if (!existsSync(tracePath)) {
    return {
      status: "WARN",
      name: "builder-pass",
      detail: "no builder pass ever recorded",
      remedy: "run /ostrom:work and confirm it records pass-ended",
    };
  }

  let source: string;
  try {
    source = readFileSync(tracePath, "utf8");
  } catch {
    return {
      status: "WARN",
      name: "builder-pass",
      detail: "builder pass history is unreadable",
      remedy: "inspect sprint.jsonl and fix its permissions",
    };
  }

  const record = lastBuilderPassEnded(source);
  if (!record) {
    return {
      status: "WARN",
      name: "builder-pass",
      detail: "no builder pass ever recorded",
      remedy: "run /ostrom:work and confirm it records pass-ended",
    };
  }

  const timestamp = record.ts;
  const timestampMs = typeof timestamp === "string" ? Date.parse(timestamp) : NaN;
  if (!Number.isFinite(timestampMs)) {
    return {
      status: "WARN",
      name: "builder-pass",
      detail: "last builder pass has an invalid timestamp",
      remedy: "inspect sprint.jsonl; records must be written by trace.sh append",
    };
  }

  const ageSeconds = nowEpoch(context) - Math.floor(timestampMs / 1000);
  if (ageSeconds > cadenceHours * 60 * 60) {
    return {
      status: "WARN",
      name: "builder-pass",
      detail: `builder pass stale, last ${timestamp} (older than ${cadenceHours}h cadence)`,
      remedy: "run /ostrom:work and confirm the recurring builder loop is active",
    };
  }
  return {
    status: "OK",
    name: "builder-pass",
    detail: `builder pass current, last ${timestamp} (${cadenceHours}h cadence)`,
    remedy: "",
  };
}
