export function attentionItems(status, tickets) {
  const items = [];
  for (const stuck of status.stuckTickets) items.push({ ticketId: stuck.ticketId, text: `Ticket ${stuck.ticketId}: stuck — ${stuck.cause ?? "cause unavailable"} (${stuck.detectedAt})` });
  for (const ticket of tickets) {
    if (ticket.state === "paused") items.push({ ticketId: ticket.id, text: `Ticket ${ticket.id}: paused — ${ticket.pauseCause ?? "cause unavailable"}` });
    if (ticket.budgetWarning) items.push({ ticketId: ticket.id, text: `Ticket ${ticket.id}: budget warning (${ticket.budgetWarning})` });
  }
  if (status.watchdog.lastSuccess === false) items.push({ text: `Watchdog read failed: ${status.watchdog.error ?? "unknown error"}` });
  const temperature = status.machine?.temperature;
  items.push({ text: `Temperature: ${temperature == null ? "unavailable" : `${temperature} °C${temperature >= 85 ? " (attention)" : ""}`}` });
  return items;
}

export function ageLabel(timestamp, now = Date.now()) {
  if (timestamp == null) return "none";
  const age = Math.max(0, Math.floor((now - Date.parse(timestamp)) / 1000));
  return `${new Date(timestamp).toLocaleString()} (${age < 60 ? `${age}s` : age < 3600 ? `${Math.floor(age / 60)}m` : `${Math.floor(age / 3600)}h`})`;
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
