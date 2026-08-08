import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import type { DoctorContext } from "../lib/context.js";
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

export function checkPublish(context: DoctorContext): CheckResult {
  const publishDir =
    context.env.MANDATE_PUBLISH_DIR ??
    join(context.configDir, "ostrom", "publish");
  const manifestPath = join(publishDir, "manifest.json");

  if (!existsSync(manifestPath)) {
    return {
      status: "WARN",
      name: "publish",
      detail: "no publish has been recorded",
      remedy: "run mandate publish.sh and confirm the state branch is reachable",
    };
  }

  let manifest: unknown;
  try {
    manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  } catch {
    return {
      status: "WARN",
      name: "publish",
      detail: "publish manifest is unreadable",
      remedy: "inspect the cached publish clone and repair or recreate it",
    };
  }

  if (!object(manifest)) {
    return {
      status: "WARN",
      name: "publish",
      detail: "publish manifest is malformed",
      remedy: "run mandate publish.sh to regenerate the cached record tree",
    };
  }

  const publishedAt = manifest.published_at;
  const cadenceHours = manifest.expected_sweep_interval_hours;
  const publishedMs =
    typeof publishedAt === "string" ? Date.parse(publishedAt) : Number.NaN;
  if (
    !Number.isFinite(publishedMs) ||
    typeof cadenceHours !== "number" ||
    !Number.isInteger(cadenceHours) ||
    cadenceHours <= 0
  ) {
    return {
      status: "WARN",
      name: "publish",
      detail: "publish manifest has invalid cadence or timestamp",
      remedy: "run mandate publish.sh to regenerate the cached record tree",
    };
  }

  const ageSeconds = nowEpoch(context) - Math.floor(publishedMs / 1000);
  if (ageSeconds > cadenceHours * 60 * 60) {
    return {
      status: "WARN",
      name: "publish",
      detail: `publish stale, last ${publishedAt} (older than ${cadenceHours}h cadence)`,
      remedy: "run mandate publish.sh and confirm the state branch is reachable",
    };
  }

  return {
    status: "OK",
    name: "publish",
    detail: `publish current, last ${publishedAt} (${cadenceHours}h cadence)`,
    remedy: "",
  };
}
