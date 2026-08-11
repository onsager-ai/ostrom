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

// One no-op is a legitimate skip -- a contended lease, a disarmed loop
// checked mid-window -- and must stay quiet. A run this long can only mean
// the loop has stopped taking ownership at all: this is the shape #73
// measured in production, 19 passes in a row, none of them noticed.
const NOOP_FAULT_THRESHOLD = 3;

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

// Walks the trace backward collecting this role's pass-ended records,
// newest first, stopping once `limit` are found. The no-op streak check
// only ever needs the most recent NOOP_FAULT_THRESHOLD, so this is the one
// backward scan both the staleness check and the fault check read from --
// scanning once from the end rather than parsing the whole (unboundedly
// growing) trace forward.
function recentRolePassEnded(
  source: string,
  role: DeliveryRole,
  limit: number,
): Record<string, unknown>[] {
  const records: Record<string, unknown>[] = [];
  let contentEnd = source.length;
  while (contentEnd > 0 && records.length < limit) {
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
    if (typeof owner === "string" && owner.startsWith(`${role}-`)) {
      records.push(record);
    }
  }
  return records;
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

  const recent = recentRolePassEnded(trace.content, role, NOOP_FAULT_THRESHOLD);
  const record = recent[0];
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

  // A run of no-ops this long means the loop is running (it stays "current"
  // on the staleness check above) but has stopped taking ownership of
  // anything -- a fault the age/cadence check alone cannot see, so it is
  // judged first and overrides an otherwise-current verdict.
  if (
    recent.length === NOOP_FAULT_THRESHOLD &&
    recent.every(
      (candidate) => object(candidate.fact) && candidate.fact.outcome === "no-op",
    )
  ) {
    return {
      status: "FAIL",
      name: checkName,
      detail: `${role} loop has produced ${NOOP_FAULT_THRESHOLD} consecutive no-op passes, last ${timestamp} (age ${age})`,
      remedy: `inspect pass-runs/${role} transcripts; the loop is running but the protocol never takes ownership`,
    };
  }

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
