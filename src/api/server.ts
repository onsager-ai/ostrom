// The daemon's local operator API. It is deliberately Unix-socket-only: Ostrom has no network
// listener and the CLI/dashboard are clients on the same machine.

import { chmod, mkdir, readFile, rm } from "node:fs/promises";
import { availableParallelism } from "node:os";
import { createServer, type Server } from "node:http";
import { dirname, join } from "node:path";
import type { Store } from "../store/store.ts";

export interface LocalApi {
  start(): Promise<void>;
  close(): Promise<void>;
}

export interface ApiControl {
  pause(ticketId: number): Promise<void>;
  resume(ticketId: number): Promise<void>;
  stop(ticketId: number): Promise<void>;
  extend(ticketId: number, increases: { timeMinutes?: number; turns?: number; tokens?: number }): Promise<void>;
  preview(): Promise<string>;
}

export interface LocalApiOptions {
  socketPath?: string;
  control: ApiControl;
  temperature?: () => Promise<number | null>;
}

export function defaultSocketPath(env: Record<string, string | undefined> = process.env): string {
  const runtime = env["XDG_RUNTIME_DIR"]?.trim();
  if (runtime !== undefined && runtime !== "") return join(runtime, "ostrom", "ostromd.sock");
  const home = env["HOME"]?.trim();
  return join(home === undefined || home === "" ? "." : home, ".local", "state", "ostrom", "ostromd.sock");
}

/** Creates the API but does not bind the socket until start. */
export function localApi(store: Store, options: LocalApiOptions): LocalApi {
  const socketPath = options.socketPath ?? defaultSocketPath();
  const server = createServer(async (request, response) => {
    const url = new URL(request.url ?? "/", "http://localhost");
    try {
      if (request.method === "GET" && url.pathname === "/v1/status") {
        return reply(response, 200, { ...store.operatorStatus(), machine: {
          slots: { total: availableParallelism(), used: store.runningAssignments().length },
          temperature: await (options.temperature ?? latestTemperature)(),
        } });
      }
      if (request.method === "GET" && url.pathname === "/v1/tickets") return reply(response, 200, store.operatorTickets());
      const ticket = /^\/v1\/tickets\/([1-9]\d*)$/.exec(url.pathname);
      if (request.method === "GET" && ticket !== null) return reply(response, 200, store.operatorTicket(Number(ticket[1])));
      if (request.method === "GET" && url.pathname === "/v1/preview") return reply(response, 200, { markdown: await options.control.preview() });
      const action = /^\/v1\/tickets\/([1-9]\d*)\/(pause|resume|stop|extend)$/.exec(url.pathname);
      if (request.method === "POST" && action !== null) {
        const ticketId = Number(action[1]);
        if (action[2] === "pause") await options.control.pause(ticketId);
        else if (action[2] === "resume") await options.control.resume(ticketId);
        else if (action[2] === "stop") await options.control.stop(ticketId);
        else if (action[2] === "extend") await options.control.extend(ticketId, await json(request));
        else return reply(response, 404, { error: "not found" });
        return reply(response, 200, store.operatorTicket(ticketId));
      }
      return reply(response, request.method === "GET" || request.method === "POST" ? 404 : 405, { error: "not found" });
    } catch (error) {
      return reply(response, 400, { error: error instanceof Error ? error.message : String(error) });
    }
  });
  return { start: () => listen(server, socketPath), close: () => close(server, socketPath) };
}

async function json(request: import("node:http").IncomingMessage): Promise<{ timeMinutes?: number; turns?: number; tokens?: number }> {
  let body = "";
  for await (const chunk of request) body += chunk;
  const parsed: unknown = JSON.parse(body);
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) throw new Error("expected a JSON object");
  const value = parsed as Record<string, unknown>;
  for (const key of ["timeMinutes", "turns", "tokens"]) {
    if (value[key] !== undefined && (!Number.isSafeInteger(value[key]) || (value[key] as number) <= 0)) {
      throw new Error(`${key} must be a positive whole number`);
    }
  }
  return value as { timeMinutes?: number; turns?: number; tokens?: number };
}

async function latestTemperature(): Promise<number | null> {
  try {
    const log = await readFile("/home/marvin/.cache/thermal-watch/temps.log", "utf8");
    const readings = [...log.matchAll(/pkg=(\d+(?:\.\d+)?)C/g)];
    return readings.length === 0 ? null : Number(readings.at(-1)![1]);
  } catch {
    return null;
  }
}

function reply(response: import("node:http").ServerResponse, status: number, body: unknown): void {
  response.writeHead(status, { "content-type": "application/json" });
  response.end(`${JSON.stringify(body)}\n`);
}

async function listen(server: Server, socketPath: string): Promise<void> {
  await mkdir(dirname(socketPath), { recursive: true, mode: 0o700 });
  // A prior daemon may have been terminated without reaching its shutdown handler.
  await rm(socketPath, { force: true });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(socketPath, () => {
      server.off("error", reject);
      resolve();
    });
  });
  await chmod(socketPath, 0o600);
}

async function close(server: Server, socketPath: string): Promise<void> {
  if (server.listening) {
    await new Promise<void>((resolve, reject) => server.close((error) => error === undefined ? resolve() : reject(error)));
  }
  await rm(socketPath, { force: true });
}
