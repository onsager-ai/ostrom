import { chmodSync, lstatSync, mkdirSync } from "node:fs";
import { createServer, request as httpRequest } from "node:http";
import type { IncomingMessage, ServerResponse } from "node:http";
import { homedir } from "node:os";
import { dirname, isAbsolute, join } from "node:path";

import { readSpendSummary } from "./budget-observer.ts";
import { generateOwnerDigest, isDigestWindow } from "./digest.ts";
import { readPlatformHealth } from "./policy-observer.ts";
import { DecisionStateError, PlatformNotFoundError, PlatformValidationError } from "./store.ts";
import type {
  AnswerDecisionInput, CreateDecisionInput, CreateLaneInput, DecisionFilter, PlatformStore,
  RecordFrictionInput, SetGoalInput, SupersedeDecisionInput, UpsertBudgetInput,
} from "./store.ts";
import type { FrictionStatus, SpendSummaryFilter } from "./types.ts";

const bodyLimit = 1024 * 1024;
type Method = "GET" | "POST" | "PUT";

export class PlatformApiError extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.name = "PlatformApiError";
    this.status = status;
  }
}

function stateDirectory(env: NodeJS.ProcessEnv): string {
  const state = env["XDG_STATE_HOME"] ?? "";
  return join(isAbsolute(state) ? state : join(homedir(), ".local", "state"), "ostrom");
}

export function defaultPlatformDbPath(env: NodeJS.ProcessEnv = process.env): string {
  return join(stateDirectory(env), "platform.sqlite");
}

export function defaultPlatformSocketPath(env: NodeJS.ProcessEnv = process.env): string {
  const runtime = env["XDG_RUNTIME_DIR"] ?? "";
  return join(isAbsolute(runtime) ? join(runtime, "ostrom") : stateDirectory(env), "platform.sock");
}

function objectBody<T extends object>(body: unknown): T {
  if (typeof body !== "object" || body === null || Array.isArray(body)) {
    throw new PlatformValidationError("expected a JSON object");
  }
  // The store validates domain fields on both the socket and direct database paths.
  return body as T;
}

export async function dispatchPlatformRequest(
  store: PlatformStore, method: string | undefined, path: string, readBody: () => Promise<unknown>,
): Promise<{ status: number; body: unknown }> {
  // Split the query before decoding identifiers so dot segments reach store validation.
  const separator = path.indexOf("?");
  const route = separator === -1 ? path : path.slice(0, separator);
  const query = new URLSearchParams(separator === -1 ? "" : path.slice(separator + 1));
  const get = method === "GET";
  const post = method === "POST";
  const put = method === "PUT";
  const ok = (body: unknown) => ({ status: 200, body });
  const created = (body: unknown) => ({ status: 201, body });
  const body = async <T extends object>() => objectBody<T>(await readBody());

  if (get && route === "/v1/health") return ok({ ok: true });
  if (get && route === "/v1/hosts") return ok(store.listT3HostStates());
  if (get && route === "/v1/waits") return ok(store.listOpenT3Waits());
  if (get && route === "/v1/prs") return ok(store.listT3ThreadPrs());
  if (get && route === "/v1/github/prs") return ok(store.listGitHubPrChecks());
  if (get && route === "/v1/spend/summary") {
    const filter = {
      scopeKind: query.get("scopeKind") ?? "team", scopeId: query.get("scopeId"),
      period: query.get("period") ?? "monthly", source: query.get("source") ?? "semon",
    } as SpendSummaryFilter;
    return ok(readSpendSummary(store, filter, new Date()));
  }
  if (get && route === "/v1/health/summary") return ok(readPlatformHealth(store, { now: new Date() }));
  if (get && route === "/v1/digest") {
    const window = query.get("window") ?? "01:00Z";
    if (!isDigestWindow(window)) throw new PlatformValidationError("invalid digest window");
    return ok(generateOwnerDigest(store, { window, now: new Date() }));
  }
  if (route === "/v1/decisions") {
    if (get) return ok(store.listDecisions(Object.fromEntries(query) as DecisionFilter));
    if (post) return created(store.createDecision(await body<CreateDecisionInput>()));
  }
  const decision = /^\/v1\/decisions\/([^/]+)(?:\/(verify|answer|decide|supersede))?$/.exec(route);
  if (decision?.[1] !== undefined) {
    const id = decodeURIComponent(decision[1]);
    const action = decision[2];
    if (get && action === "verify") return ok(store.verifyDecision(id));
    if (get && action === undefined) {
      const entry = store.getDecision(id);
      if (entry === null) throw new PlatformNotFoundError(`decision ${id} not found`);
      return ok(entry);
    }
    if (post && (action === "answer" || action === "decide")) {
      // M1 reserves the 0700-directory/0600 write socket and direct SQLite access for the
      // operator/Chief PM OS account. decidedBy is an attestation. Derive caller identity
      // before allowing Manager writes from other principals.
      return ok(store.answerDecision(id, await body<AnswerDecisionInput>()));
    }
    if (post && action === "supersede") return ok(store.supersedeDecision(id, await body<SupersedeDecisionInput>()));
  }
  if (route === "/v1/lanes") {
    if (get) return ok(store.listLanes());
    if (post) return created(store.createLane(await body<CreateLaneInput>()));
  }
  const lane = /^\/v1\/lanes\/([^/]+)(\/goal)?$/.exec(route);
  if (lane?.[1] !== undefined && ((get && lane[2] === undefined) || (put && lane[2] !== undefined))) {
    const slug = decodeURIComponent(lane[1]);
    const entry = store.getLaneBySlug(slug);
    if (entry === null) throw new PlatformNotFoundError(`lane ${slug} not found`);
    if (get) return ok({ ...entry, goal: store.getLatestGoal(entry.id) });
    return ok(store.setGoal({ ...await body<Omit<SetGoalInput, "laneId">>(), laneId: entry.id }));
  }
  if (get && route === "/v1/policy/rules") return ok(store.listPolicyRules());
  if (get && route === "/v1/policy/events") return ok(store.listPolicyEvents(Object.fromEntries(query)));
  if (route === "/v1/friction") {
    if (get) return ok(store.listFriction(Object.fromEntries(query) as { status?: FrictionStatus }));
    if (post) return created(store.recordFriction(await body<RecordFrictionInput>()));
  }
  if (route === "/v1/budgets") {
    if (get) return ok(store.listBudgets());
    if (put) return ok(store.upsertBudget(await body<UpsertBudgetInput>()));
  }
  if (get && route === "/v1/agents") return ok(store.listAgents());
  if (get && route === "/v1/runs") return ok(store.listRuns());
  throw new PlatformNotFoundError("route not found");
}

async function requestBody(request: IncomingMessage): Promise<unknown> {
  const tooLarge = () => new PlatformApiError(413, "request body exceeds 1 MiB");
  if (Number(request.headers["content-length"]) > bodyLimit) throw tooLarge();
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of request.iterator({ destroyOnReturn: false }) as AsyncIterable<Buffer>) {
    size += chunk.length;
    if (size > bodyLimit) throw tooLarge();
    chunks.push(chunk);
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8")) as unknown;
}

function reply(response: ServerResponse, status: number, body: unknown): void {
  response.setHeader("content-type", "application/json");
  response.statusCode = status;
  response.end(`${JSON.stringify(body)}\n`);
}

function sqliteStatus(error: Error): number | undefined {
  if (!("code" in error) || error.code !== "ERR_SQLITE_ERROR" || !("errcode" in error)) return undefined;
  switch (error.errcode) {
    case 1555: // SQLITE_CONSTRAINT_PRIMARYKEY
    case 2067: // SQLITE_CONSTRAINT_UNIQUE
      return 409;
    case 1811: // SQLITE_CONSTRAINT_TRIGGER
      return error.message === "decision already exists" ? 409 : undefined;
    case 275: // SQLITE_CONSTRAINT_CHECK
    case 787: // SQLITE_CONSTRAINT_FOREIGNKEY
    case 1299: // SQLITE_CONSTRAINT_NOTNULL
    case 3091: // SQLITE_CONSTRAINT_DATATYPE
      return 400;
    default:
      return undefined;
  }
}

function apiError(error: unknown): PlatformApiError {
  if (error instanceof PlatformApiError) return error;
  if (error instanceof PlatformValidationError || error instanceof SyntaxError || error instanceof URIError) {
    return new PlatformApiError(400, error.message);
  }
  if (error instanceof PlatformNotFoundError) return new PlatformApiError(404, error.message);
  if (error instanceof DecisionStateError) return new PlatformApiError(409, error.message);
  if (error instanceof Error) {
    const status = sqliteStatus(error);
    if (status !== undefined) return new PlatformApiError(status, error.message);
  }
  return new PlatformApiError(500, "internal platform error");
}

export interface PlatformApiServer {
  close(): Promise<void>;
}

export async function startPlatformApiServer({ store, socketPath }: {
  store: PlatformStore;
  socketPath: string;
}): Promise<PlatformApiServer> {
  const parent = dirname(socketPath);
  mkdirSync(parent, { recursive: true, mode: 0o700 });
  const stat = lstatSync(parent);
  const uid = process.getuid?.();
  if (stat.isSymbolicLink() || !stat.isDirectory() || (stat.mode & 0o077) !== 0
    || (uid !== undefined && stat.uid !== uid)) {
    throw new PlatformValidationError("socket parent must be a private directory owned by the current user");
  }
  const server = createServer(async (request, response) => {
    try {
      const result = await dispatchPlatformRequest(store, request.method, request.url ?? "/", () => requestBody(request));
      reply(response, result.status, result.body);
    } catch (error) {
      const failure = apiError(error);
      if (failure.status === 413) {
        response.setHeader("connection", "close");
        request.resume();
      }
      reply(response, failure.status, { error: failure.message });
    }
  });

  const close = () => new Promise<void>((resolve, reject) => {
    server.close((error) => error ? reject(error) : resolve());
  });

  // Binding fails if another server owns this path. Node removes its own socket on close.
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(socketPath, () => {
      server.off("error", reject);
      try {
        // Restrict access before yielding to any accepted requests.
        chmodSync(socketPath, 0o600);
        resolve();
      } catch (error) {
        void close().then(() => reject(error), reject);
      }
    });
  });
  return { close };
}

export class PlatformApiClient {
  readonly socketPath: string;

  constructor(socketPath: string) {
    this.socketPath = socketPath;
  }

  request<T = unknown>(method: Method, path: string, body?: unknown): Promise<T> {
    return new Promise((resolve, reject) => {
      const encoded = body === undefined ? undefined : JSON.stringify(body);
      const headers = encoded === undefined ? {} : {
        "content-type": "application/json",
        "content-length": Buffer.byteLength(encoded),
      };
      const request = httpRequest({ socketPath: this.socketPath, method, path, headers }, (response) => {
        let text = "";
        response.setEncoding("utf8");
        response.on("data", (chunk: string) => { text += chunk; });
        response.once("error", reject);
        response.once("end", () => {
          try {
            const result: unknown = JSON.parse(text);
            const status = response.statusCode ?? 500;
            if (status >= 200 && status < 300) resolve(result as T);
            else {
              const message = typeof result === "object" && result !== null && "error" in result
                && typeof result.error === "string" ? result.error : `HTTP ${status}`;
              reject(new PlatformApiError(status, message));
            }
          } catch (error) {
            reject(error);
          }
        });
      });
      request.once("error", reject);
      request.end(encoded);
    });
  }
}
