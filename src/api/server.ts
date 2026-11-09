// The daemon's local operator API. It is deliberately Unix-socket-only: Ostrom has no network
// listener and the CLI/dashboard are clients on the same machine.

import { chmod, mkdir, rm } from "node:fs/promises";
import { createServer, type Server } from "node:http";
import { dirname, join } from "node:path";
import type { Store } from "../store/store.ts";

export interface LocalApi {
  start(): Promise<void>;
  close(): Promise<void>;
}

export function defaultSocketPath(env: Record<string, string | undefined> = process.env): string {
  const runtime = env["XDG_RUNTIME_DIR"]?.trim();
  if (runtime !== undefined && runtime !== "") return join(runtime, "ostrom", "ostromd.sock");
  const home = env["HOME"]?.trim();
  if (home === undefined || home === "") throw new Error("HOME is required when XDG_RUNTIME_DIR is unset");
  return join(home, ".local", "state", "ostrom", "ostromd.sock");
}

/** Creates the API but does not bind the socket until start. */
export function localApi(store: Store, socketPath = defaultSocketPath()): LocalApi {
  const server = createServer((request, response) => {
    const url = new URL(request.url ?? "/", "http://localhost");
    if (request.method !== "GET") return reply(response, 405, { error: "method not allowed" });
    try {
      if (url.pathname === "/v1/status") return reply(response, 200, store.operatorStatus());
      if (url.pathname === "/v1/tickets") return reply(response, 200, store.operatorTickets());
      const ticket = /^\/v1\/tickets\/([1-9]\d*)$/.exec(url.pathname);
      if (ticket !== null) return reply(response, 200, store.operatorTicket(Number(ticket[1])));
      return reply(response, 404, { error: "not found" });
    } catch (error) {
      return reply(response, 404, { error: error instanceof Error ? error.message : String(error) });
    }
  });
  return { start: () => listen(server, socketPath), close: () => close(server, socketPath) };
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
