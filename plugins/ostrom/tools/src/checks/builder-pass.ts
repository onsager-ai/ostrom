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
const PASS_FAULT_THRESHOLD = 3;

const OUTPUT_KINDS = new Set([
  "work-dispatched",
  "decision-taken",
  "pr-repair",
]);

interface RecentRolePass {
  record: Record<string, unknown>;
  terminalOutput: boolean;
  outputRecordSeen: boolean;
  owner: string;
  started: boolean;
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

function hasNonZeroTerminalCount(fact: Record<string, unknown>): boolean {
  return [fact.worked_items, fact.completed_candidates].some(
    (count) => typeof count === "number" && Number.isFinite(count) && count > 0,
  );
}

// Walks the trace backward collecting this role's pass-ended records, newest
// first. Once `limit` ends are found, the scan continues only far enough to
// reach their matching pass-started rows. Output records encountered inside
// each boundary mark that pass productive; the terminal counts emitted by the
// two role protocols are a fallback for traces recorded before those output
// kinds existed. This keeps both the age and fault checks on one bounded scan
// from the end instead of parsing the unbounded trace forward.
function recentRolePassEnded(
  source: string,
  role: DeliveryRole,
  limit: number,
): RecentRolePass[] {
  const passes: RecentRolePass[] = [];
  let contentEnd = source.length;
  while (
    contentEnd > 0 &&
    (passes.length < limit || passes.some((pass) => !pass.started))
  ) {
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
    if (!object(record)) continue;

    if (record.kind === "pass-ended" && object(record.fact)) {
      const owner = record.fact.owner;
      if (
        passes.length < limit &&
        typeof owner === "string" &&
        owner.startsWith(`${role}-`)
      ) {
        passes.push({
          record,
          terminalOutput: hasNonZeroTerminalCount(record.fact),
          outputRecordSeen: false,
          owner,
          started: false,
        });
      }
      continue;
    }

    const openPasses = passes.filter((pass) => !pass.started);
    if (OUTPUT_KINDS.has(String(record.kind))) {
      for (const pass of openPasses) pass.outputRecordSeen = true;
    }

    if (record.kind === "pass-started" && object(record.fact)) {
      const owner = record.fact.owner;
      if (typeof owner !== "string") continue;
      const matchingPass = openPasses.find((pass) => pass.owner === owner);
      if (matchingPass) matchingPass.started = true;
    }
  }
  return passes;
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

  const recent = recentRolePassEnded(trace.content, role, PASS_FAULT_THRESHOLD);
  const newest = recent[0];
  if (!newest) {
    return {
      status: "WARN",
      name: checkName,
      detail: `no ${role} pass ever recorded`,
      remedy: `run ${ROLE_SKILL[role]} and confirm it records pass-ended`,
    };
  }
  const record = newest.record;

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
  const producedNothing =
    recent.length === PASS_FAULT_THRESHOLD &&
    recent.every(
      (candidate) =>
        !candidate.terminalOutput &&
        !(candidate.started && candidate.outputRecordSeen),
    );

  // A run of no-ops this long means the loop is running (it stays "current"
  // on the staleness check above) but has stopped taking ownership of
  // anything -- a fault the age/cadence check alone cannot see, so it is
  // judged first and overrides an otherwise-current verdict.
  if (
    producedNothing &&
    recent.every(
      (candidate) =>
        object(candidate.record.fact) &&
        candidate.record.fact.outcome === "no-op",
    )
  ) {
    return {
      status: "FAIL",
      name: checkName,
      detail: `${role} loop has produced ${PASS_FAULT_THRESHOLD} consecutive no-op passes, last ${timestamp} (age ${age})`,
      remedy: `inspect pass-runs/${role} transcripts; the loop is running but the protocol never takes ownership`,
    };
  }

  // The protocol did take ownership in this shape, but repeatedly reported
  // that it failed. Treating those fresh wrapper rows as healthy would hide
  // the same dead loop as a no-op streak, one layer deeper.
  if (
    producedNothing &&
    recent.every(
      (candidate) =>
        object(candidate.record.fact) &&
        candidate.record.fact.outcome === "failed",
    )
  ) {
    return {
      status: "FAIL",
      name: checkName,
      detail: `${role} loop has produced ${PASS_FAULT_THRESHOLD} consecutive failed passes, last ${timestamp} (age ${age})`,
      remedy: `inspect pass-runs/${role} transcripts; the protocol takes ownership but does not complete`,
    };
  }

  if (producedNothing) {
    return {
      status: "FAIL",
      name: checkName,
      detail: `${role} loop has produced no output for ${PASS_FAULT_THRESHOLD} consecutive passes, last ${timestamp} (age ${age})`,
      remedy: `inspect pass-runs/${role} transcripts and the queue; the protocol runs but dispatches no work, records no decision, and repairs no pull request`,
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
