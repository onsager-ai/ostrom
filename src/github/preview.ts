// Markdown preview of the next outbox send pass. It is deliberately read-only: the same pending
// records and batching rules feed the real sender, so developers can inspect delivery before a tick.

import type { Reply, Store } from "../store/store.ts";

export function renderPreview(store: Store, now = new Date()): string {
  const rows = store.pendingReplies();
  if (rows.length === 0) return "# Ostrom preview\n\nNothing to send.\n";
  return ["# Ostrom preview", "", ...rows.map((reply) => render(reply, store, now))].join("\n\n");
}

function render(reply: Reply, store: Store, now: Date): string {
  const thread = store.getThread(reply.threadId);
  const destination = `${thread.url} (${thread.kind} #${thread.number})`;
  if (reply.kind === "comment") return `## Comment on ${destination}\n\n${reply.body}`;
  if (reply.kind === "edit") return `## Edit status card on ${destination}\n\n${reply.body}`;
  if (reply.kind === "reaction") return `## React ${reply.reaction} on ${reply.target}\n\nReceived ${minutesAgo(reply.enqueuedAt, now)} min ago.`;
  return `## Label on ${destination}\n\n${reply.body}`;
}

function minutesAgo(at: string, now: Date): number {
  return Math.max(0, Math.floor((now.getTime() - Date.parse(at)) / 60_000));
}
