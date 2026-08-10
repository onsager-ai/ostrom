import type { DoctorContext } from "../lib/context.js";
import type { CheckResult } from "../lib/result.js";

type DeliveryRole = "builder" | "gatekeeper";

// Builder scheduling has three-hour overnight gaps; gatekeeping is hourly.
// These are delivery-loop cadences, not mandate sweep cadence_hours.
const CADENCE_HOURS: Record<DeliveryRole, number> = {
  builder: 3,
  gatekeeper: 1,
};

const ROLE_SKILL: Record<DeliveryRole, string> = {
  builder: "/ostrom:work",
  gatekeeper: "/ostrom:gatekeep",
};

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

function formatAge(ageSeconds: number): string {
  const ageMinutes = Math.max(0, Math.floor(ageSeconds / 60));
  if (ageMinutes < 60) return `${ageMinutes}m`;
  const hours = Math.floor(ageMinutes / 60);
  const minutes = ageMinutes % 60;
  return minutes === 0 ? `${hours}h` : `${hours}h${minutes}m`;
}

function lastRolePassEnded(
  source: string,
  role: DeliveryRole,
): Record<string, unknown> | undefined {
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
    if (typeof owner === "string" && owner.startsWith(`${role}-`)) return record;
  }
  return undefined;
}

function checkRolePass(
  context: DoctorContext,
  role: DeliveryRole,
): CheckResult {
  const cadenceHours = CADENCE_HOURS[role];
  const checkName = `${role}-pass`;
  const trace = context.readTrace();
  if (!trace.exists) {
    return {
      status: "WARN",
      name: checkName,
      detail: `no ${role} pass ever recorded`,
      remedy: `run ${ROLE_SKILL[role]} and confirm it records pass-ended`,
    };
  }
  if (!("content" in trace)) {
    return {
      status: "WARN",
      name: checkName,
      detail: `${role} pass history is unreadable`,
      remedy: "inspect sprint.jsonl and fix its permissions",
    };
  }

  const record = lastRolePassEnded(trace.content, role);
  if (!record) {
    return {
      status: "WARN",
      name: checkName,
      detail: `no ${role} pass ever recorded`,
      remedy: `run ${ROLE_SKILL[role]} and confirm it records pass-ended`,
    };
  }

  const timestamp = record.ts;
  const timestampMs = typeof timestamp === "string" ? Date.parse(timestamp) : NaN;
  if (!Number.isFinite(timestampMs)) {
    return {
      status: "WARN",
      name: checkName,
      detail: `last ${role} pass has an invalid timestamp`,
      remedy: "inspect sprint.jsonl; records must be written by trace.sh append",
    };
  }

  const ageSeconds = nowEpoch(context) - Math.floor(timestampMs / 1000);
  const age = formatAge(ageSeconds);
  if (ageSeconds > cadenceHours * 60 * 60) {
    return {
      status: "WARN",
      name: checkName,
      detail: `${role} pass stale, last ${timestamp} (age ${age}; older than ${cadenceHours}h cadence)`,
      remedy: `confirm ostrom-${role}-pass.timer is active and loop-armed is present`,
    };
  }
  return {
    status: "OK",
    name: checkName,
    detail: `${role} pass current, last ${timestamp} (age ${age}; ${cadenceHours}h cadence)`,
    remedy: "",
  };
}

export function checkBuilderPass(context: DoctorContext): CheckResult {
  return checkRolePass(context, "builder");
}

export function checkGatekeeperPass(context: DoctorContext): CheckResult {
  return checkRolePass(context, "gatekeeper");
}
