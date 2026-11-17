/** Project the stored state into the operator's next useful answer. Order matters. */
export function ticketPhase(ticket) {
  if (ticket.state === "ended") return { key: "finished", label: "Finished", tone: "neutral", detail: "" };
  if (ticket.state === "paused") return { key: "paused", label: "Paused", tone: "amber", detail: ticket.pauseCause ?? "Cause unavailable" };
  if (ticket.assignment?.outcome === "failed" || ticket.latestFailure) return {
    key: "failed", label: "Failed", tone: "red",
    detail: ticket.latestFailure?.summary ?? ticket.assignment?.lastError ?? ticket.assignment?.endReason ?? "Failure details unavailable",
  };
  if (ticket.assignment?.turnStartedAt) return {
    key: "working", label: "Working", tone: "blue",
    detail: `Turn ${ticket.assignment.turns} of ${ticket.budget.turnLimit}`,
  };
  if (!ticket.pullRequests.length) return { key: "working", label: "Working, no PR yet", tone: "blue", detail: "" };
  const reason = ticket.mergeCandidate?.blockingReason;
  if (!reason) return { key: "ready", label: "Ready to merge", tone: "green", detail: "" };
  const lower = reason.toLowerCase();
  if (lower.includes("approvals are not satisfied")) return { key: "approval", label: "Waiting for approval", tone: "amber", detail: "Only submitted reviews count; check GitHub for an unsubmitted draft." };
  if (lower.includes("quiet period")) return {
    key: "quiet", label: ticket.mergeCandidate.quietDeadline ? `Approved, merges after ${localTime(ticket.mergeCandidate.quietDeadline)}` : "Approved, waiting for quiet period",
    tone: "green", detail: "",
  };
  if (lower.includes("not clean")) return { key: "ci", label: "Waiting for CI or a clean merge", tone: "amber", detail: "" };
  if (lower.includes("not idle")) return { key: "revising", label: "Worker revising", tone: "blue", detail: "" };
  return { key: "blocked", label: "Blocked", tone: "neutral", detail: reason };
}

export function phaseSummary(tickets) {
  const names = { working: "working", approval: "waiting for approval", quiet: "approved, waiting to merge", ci: "waiting for CI or a clean merge", revising: "revising", ready: "ready to merge", paused: "paused", failed: "failed", blocked: "blocked" };
  const counts = new Map();
  for (const ticket of tickets.filter(t => t.state !== "ended")) {
    const { key } = ticketPhase(ticket);
    counts.set(key, (counts.get(key) ?? 0) + 1);
  }
  return [...counts].map(([key, count]) => `${count} ${names[key]}`).join(" · ") || "No active tickets";
}

export function ticketSummary(detail) {
  const latest = [...(detail?.decisions ?? [])].sort((a, b) => Date.parse(b.decidedAt) - Date.parse(a.decidedAt) || b.id - a.id)[0];
  const full = latest?.reason ?? detail?.context?.reason ?? "Waiting for a decision summary";
  return { full, short: full.length > 120 ? `${full.slice(0, 119).trimEnd()}…` : full };
}

export function budgetMeters(ticket) {
  return [
    ["Time", ticket.budgetUsage.time, ticket.budget.timeMinutes, "min"],
    ["Turns", ticket.budgetUsage.turns, ticket.budget.turnLimit, ""],
    ["Tokens", ticket.budgetUsage.tokens, ticket.budget.tokenLimit, "%"],
  ].map(([label, used, limit, unit]) => {
    const percent = limit > 0 ? used / limit * 100 : null;
    const rounded = Math.round((percent ?? 0) * 10) / 10;
    return { label, used, limit, percent, warning: percent !== null && percent >= 80,
      text: unit === "%" ? (percent === null ? "No limit" : `${rounded}% of limit`) : `${used} / ${limit} ${unit}`.trim(),
    };
  });
}

export function attentionItems(status, tickets) {
  const items = [];
  for (const stuck of status.stuckTickets) items.push({ ticketId: stuck.ticketId, text: `Stalled: ${stuck.cause ?? "cause unavailable"}` });
  for (const ticket of tickets.filter(t => t.state !== "ended")) {
    if (ticket.state === "paused") items.push({ ticketId: ticket.id, text: `Paused: ${ticket.pauseCause ?? "cause unavailable"}` });
    if (ticket.assignment?.outcome === "failed" || ticket.latestFailure) items.push({ ticketId: ticket.id, text: `Failed: ${ticket.latestFailure?.summary ?? ticket.assignment?.lastError ?? ticket.assignment?.endReason ?? "details unavailable"}` });
    if (ticket.budgetWarning) items.push({ ticketId: ticket.id, text: `Budget warning: ${ticket.budgetWarning}` });
  }
  if (status.watchdog.lastSuccess === false) items.push({ text: `Watchdog read failed: ${status.watchdog.error ?? "unknown error"}` });
  const temperature = status.machine?.temperature;
  if (temperature != null && temperature >= 75) items.push({ text: `Machine hot: ${temperature} °C` });
  return items;
}

/** Merge policy is stored as JSON, e.g. {"strategy":"approvals","approvals":1,"approvers":["a"]}. */
export function policyLabel(policy) {
  try {
    const p = JSON.parse(policy);
    if (p.strategy !== "approvals") return String(p.strategy);
    return `approvals: ${p.approvals} from ${p.approvers?.length ? p.approvers.join(", ") : "any writer"}`;
  } catch { return policy; }
}

export function localTime(timestamp) {
  return timestamp == null ? "Unavailable" : new Date(timestamp).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

export function ageLabel(timestamp, now = Date.now()) {
  if (timestamp == null) return "No completed turn yet";
  const seconds = Math.max(0, Math.floor((now - Date.parse(timestamp)) / 1000));
  if (seconds < 60) return "Just now";
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} hr ago`;
  const days = Math.floor(seconds / 86400);
  return `${days} ${days === 1 ? "day" : "days"} ago`;
}

export function githubUrl(value) {
  try {
    const url = new URL(value);
    return url.protocol === "https:" && (url.hostname === "github.com" || url.hostname === "www.github.com") ? url.href : null;
  } catch { return null; }
}

export function createRefresh(fetcher, onSuccess, onFailure) {
  let busy = false;
  let lastSnapshot;
  return async function refresh() {
    if (busy) return;
    busy = true;
    try {
      const snapshot = await fetcher();
      lastSnapshot = snapshot;
      onSuccess(snapshot, new Date());
    } catch (error) {
      onFailure(lastSnapshot, error);
    } finally { busy = false; }
  };
}
