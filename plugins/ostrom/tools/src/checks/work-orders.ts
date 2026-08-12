import { spawnSync } from "node:child_process";
import type { DoctorContext } from "../lib/context.js";
import type { CheckResult } from "../lib/result.js";

interface DispatchFact {
  schema_version: 1;
  item_id: string;
  order_id: string;
  unit_name: string;
  backend: string;
}

function object(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function dispatchFact(value: unknown): value is DispatchFact {
  return (
    object(value) &&
    value.schema_version === 1 &&
    typeof value.item_id === "string" &&
    value.item_id.length > 0 &&
    typeof value.order_id === "string" &&
    value.order_id.length > 0 &&
    typeof value.unit_name === "string" &&
    value.unit_name.length > 0 &&
    typeof value.backend === "string" &&
    value.backend.length > 0
  );
}

function inFlight(source: string): DispatchFact[] {
  const dispatched = new Map<string, DispatchFact>();
  const terminal = new Set<string>();

  for (const line of source.split(/\r?\n/)) {
    if (!line) continue;
    let record: unknown;
    try {
      record = JSON.parse(line);
    } catch {
      continue;
    }
    if (!object(record) || !object(record.fact)) continue;
    if (record.kind === "work-dispatched" && dispatchFact(record.fact)) {
      dispatched.set(record.fact.order_id, record.fact);
    } else if (
      (record.kind === "work-completed" || record.kind === "work-failed") &&
      typeof record.fact.order_id === "string"
    ) {
      terminal.add(record.fact.order_id);
    }
  }

  return [...dispatched.entries()]
    .filter(([orderId]) => !terminal.has(orderId))
    .map(([, fact]) => fact);
}

function systemdUnitState(
  context: DoctorContext,
  unitName: string,
): string | null | undefined {
  const systemctl = context.env.MANDATE_SYSTEMCTL_BIN || "systemctl";
  const result = spawnSync(
    systemctl,
    ["--user", "show", `${unitName}.service`, "--property=ActiveState", "--value"],
    { encoding: "utf8", env: context.env },
  );
  if (result.status === 4) return null;
  if (result.status !== 0) return undefined;
  const state = result.stdout.trim();
  return state || null;
}

export function checkWorkOrders(context: DoctorContext): CheckResult {
  const trace = context.readTrace();
  if (!trace.exists || !("content" in trace)) {
    return {
      status: "OK",
      name: "work-orders",
      detail: "no work orders in flight",
      remedy: "",
    };
  }

  const orders = inFlight(trace.content);
  if (orders.length === 0) {
    return {
      status: "OK",
      name: "work-orders",
      detail: "no work orders in flight",
      remedy: "",
    };
  }

  const faults: DispatchFact[] = [];
  const unknown: DispatchFact[] = [];
  const visible: string[] = [];
  for (const order of orders) {
    visible.push(`${order.item_id} (${order.unit_name})`);
    if (order.backend !== "systemd") continue;
    const state = systemdUnitState(context, order.unit_name);
    if (state === undefined) {
      unknown.push(order);
    } else if (
      !state ||
      !["active", "activating", "reloading", "deactivating"].includes(state)
    ) {
      faults.push(order);
    }
  }

  if (faults.length > 0) {
    return {
      status: "FAIL",
      name: "work-orders",
      detail: `${orders.length} in flight; unit exited without terminal row: ${faults
        .map((order) => `${order.item_id} (${order.unit_name})`)
        .join(", ")}`,
      remedy:
        "inspect the transient unit journal and append work-failed before clearing its per-item lease",
    };
  }

  if (unknown.length > 0) {
    return {
      status: "WARN",
      name: "work-orders",
      detail: `${orders.length} in flight; could not inspect unit state: ${unknown
        .map((order) => `${order.item_id} (${order.unit_name})`)
        .join(", ")}`,
      remedy: "confirm the user systemd manager is reachable and inspect the transient unit",
    };
  }

  return {
    status: "OK",
    name: "work-orders",
    detail: `${orders.length} in flight: ${visible.join(", ")}`,
    remedy: "",
  };
}
