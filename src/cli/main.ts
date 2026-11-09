// The operator CLI. It uses ostromd's Unix-socket API; when the daemon is not running, read-only
// views can still inspect the SQLite store directly.

import { request } from "node:http";

import { defaultSocketPath } from "../api/server.ts";
import { renderPreview } from "../github/preview.ts";
import { Store, type OperatorStatus, type OperatorTicket, type OperatorTicketDetail } from "../store/store.ts";

const USAGE = `usage: ostrom <command> [--json]

commands:
  status
  tickets
  show <ticket>
  pause <ticket>
  resume <ticket>
  stop <ticket>
  extend <ticket> --time M [--turns N] [--tokens N]
  preview

Read-only commands fall back to OSTROM_DB when ostromd is not running.`;

type Command =
  | { name: "status" | "tickets" | "preview"; json: boolean }
  | { name: "show" | "pause" | "resume" | "stop"; ticketId: number; json: boolean }
  | { name: "extend"; ticketId: number; increases: { timeMinutes?: number; turns?: number; tokens?: number }; json: boolean };

export async function main(argv: readonly string[], env: Record<string, string | undefined> = process.env): Promise<number> {
  const command = parse(argv);
  if (command === null) return usage();
  try {
    const result = await api(command, env);
    output(command, result);
    return 0;
  } catch (error) {
    if (!readOnly(command) || !unavailable(error)) return failure(error);
    const path = env["OSTROM_DB"]?.trim() ?? "";
    if (path === "") return failure("ostromd is not running; set OSTROM_DB for read-only fallback");
    try {
      const result = direct(command, path);
      output(command, result);
      return 0;
    } catch (fallback) {
      return failure(fallback);
    }
  }
}

function usage(): number {
  process.stderr.write(`${USAGE}\n`);
  return 2;
}

function failure(error: unknown): number {
  process.stderr.write(`ostrom: ${error instanceof Error ? error.message : String(error)}\n`);
  return 1;
}

function parse(argv: readonly string[]): Command | null {
  const json = argv.at(-1) === "--json";
  const args = json ? argv.slice(0, -1) : argv;
  if (args.length === 1 && (args[0] === "status" || args[0] === "tickets" || args[0] === "preview")) return { name: args[0], json };
  if (args.length === 2 && (args[0] === "show" || args[0] === "pause" || args[0] === "resume" || args[0] === "stop") && /^[1-9]\d*$/.test(args[1]!)) {
    return { name: args[0], ticketId: Number(args[1]), json };
  }
  if (args[0] !== "extend" || !/^[1-9]\d*$/.test(args[1] ?? "")) return null;
  const increases: { timeMinutes?: number; turns?: number; tokens?: number } = {};
  for (let index = 2; index < args.length; index += 2) {
    const flag = args[index];
    const value = args[index + 1] ?? "";
    if (!/^[1-9]\d*$/.test(value)) return null;
    if (flag === "--time") increases.timeMinutes = Number(value);
    else if (flag === "--turns") increases.turns = Number(value);
    else if (flag === "--tokens") increases.tokens = Number(value);
    else return null;
  }
  return Object.keys(increases).length === 0 ? null : { name: "extend", ticketId: Number(args[1]), increases, json };
}

async function api(command: Command, env: Record<string, string | undefined>): Promise<unknown> {
  const socketPath = defaultSocketPath(env);
  if (command.name === "status") return call(socketPath, "GET", "/v1/status");
  if (command.name === "tickets") return call(socketPath, "GET", "/v1/tickets");
  if (command.name === "show") return call(socketPath, "GET", `/v1/tickets/${command.ticketId}`);
  if (command.name === "preview") return call(socketPath, "GET", "/v1/preview");
  if (command.name === "extend") return call(socketPath, "POST", `/v1/tickets/${command.ticketId}/extend`, command.increases);
  return call(socketPath, "POST", `/v1/tickets/${command.ticketId}/${command.name}`);
}

function call(socketPath: string, method: "GET" | "POST", path: string, body?: unknown): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const encoded = body === undefined ? undefined : JSON.stringify(body);
    const client = request({ socketPath, path, method, headers: encoded === undefined ? undefined : { "content-type": "application/json", "content-length": Buffer.byteLength(encoded) } }, (response) => {
      let text = "";
      response.setEncoding("utf8");
      response.on("data", (chunk: string) => { text += chunk; });
      response.on("end", () => {
        try {
          const parsed = JSON.parse(text) as { error?: string };
          if (response.statusCode !== 200) reject(new Error(parsed.error ?? `daemon returned ${response.statusCode}`));
          else resolve(parsed);
        } catch (error) { reject(error); }
      });
    });
    client.once("error", reject);
    client.end(encoded);
  });
}

function readOnly(command: Command): boolean {
  return command.name === "status" || command.name === "tickets" || command.name === "show" || command.name === "preview";
}

function unavailable(error: unknown): boolean {
  return error instanceof Error && ["ECONNREFUSED", "ENOENT", "ENOTSOCK"].some((code) => error.message.includes(code));
}

function direct(command: Command, path: string): unknown {
  const store = Store.open(path);
  try {
    if (command.name === "status") return store.operatorStatus();
    if (command.name === "tickets") return store.operatorTickets();
    if (command.name === "show") return store.operatorTicket(command.ticketId);
    if (command.name === "preview") return { markdown: renderPreview(store) };
    throw new Error("mutating commands require ostromd to be running");
  } finally {
    store.close();
  }
}

function output(command: Command, value: unknown): void {
  if (command.json) return process.stdout.write(`${JSON.stringify(value)}\n`);
  if (command.name === "status") return status(value as OperatorStatus);
  if (command.name === "tickets") return tickets(value as OperatorTicket[]);
  if (command.name === "show" || command.name === "pause" || command.name === "resume" || command.name === "stop" || command.name === "extend") return show(value as OperatorTicketDetail);
  process.stdout.write((value as { markdown: string }).markdown);
}

function status(view: OperatorStatus): void {
  process.stdout.write(`tickets  open ${view.tickets.open}  paused ${view.tickets.paused}  ended ${view.tickets.ended}\n`);
  process.stdout.write(`workers  running ${view.runningWorkers.length}\n`);
  process.stdout.write(`stuck    ${view.stuckTickets.length}\n`);
  process.stdout.write(`tokens   ${view.tokensToday} today\n`);
}

function tickets(rows: OperatorTicket[]): void {
  process.stdout.write("TICKET  STATE   WORKER  SESSION\n");
  for (const ticket of rows) process.stdout.write(`${String(ticket.id).padEnd(7)} ${ticket.state.padEnd(7)} ${(ticket.assignment?.harness ?? "-").padEnd(7)} ${ticket.assignment?.sessionId ?? "-"}\n`);
}

function show(ticket: OperatorTicketDetail): void {
  process.stdout.write(`ticket ${ticket.id}  ${ticket.state}\n`);
  process.stdout.write(`thread  ${ticket.thread.url}\n`);
  process.stdout.write(`budget  turns ${ticket.budget.turns}/${ticket.budget.turnLimit}  tokens ${ticket.budget.inputTokens + ticket.budget.outputTokens}/${ticket.budget.tokenLimit}\n`);
  if (ticket.assignment?.sessionId !== null && ticket.assignment?.sessionId !== undefined) process.stdout.write(`session ${ticket.assignment.sessionId}\nsemon query read_transcript ${ticket.assignment.sessionId}\n`);
  for (const decision of ticket.decisions) process.stdout.write(`decision ${decision.id}  ${decision.verdict}  ${decision.reason}\n`);
  for (const trigger of ticket.triggers) process.stdout.write(`trigger ${trigger.id}  ${trigger.kind}  ${trigger.url}\n`);
  for (const assignment of ticket.assignments) process.stdout.write(`assignment ${assignment.id}  ${assignment.state}  ${assignment.outcome ?? "-"}\n`);
  for (const reply of ticket.outbox) process.stdout.write(`outbox ${reply.id}  ${reply.kind}  ${reply.sentAt === null ? "pending" : "sent"}\n`);
}

if (import.meta.main) process.exitCode = await main(process.argv.slice(2));
