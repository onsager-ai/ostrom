#!/usr/bin/env node

import { inspect } from "node:util";

import {
  approveWithPairingCode, buildAuthorizationRequest, defaultT3TokenDirectory,
  discoverOAuthMetadata, exchangeAuthorizationCode, generatePkcePair, registerOAuthClient,
  saveToken, waitForLoopbackCallback,
} from "../connectors/t3/index.ts";

import {
  defaultPlatformDbPath, defaultPlatformSocketPath, dispatchPlatformRequest,
  PlatformApiClient, startPlatformApiServer,
} from "./api.ts";
import { defaultHostsConfigPath, loadPlatformHosts } from "./config.ts";
import type { PlatformT3Host } from "./config.ts";
import { startPlatformDashboard } from "./dashboard.ts";
import { isDigestWindow } from "./digest.ts";
import { evaluatePolicyAndHealth } from "./policy-observer.ts";
import { PlatformStore } from "./store.ts";
import { T3HostSupervisor } from "./t3-supervisor.ts";
import { decisionCategories } from "./types.ts";
import type { DecisionVerification, Lane } from "./types.ts";

const usage = `usage: ostrom-platform [--db <path> | --socket <path>] [--json] <command>

  decision create --title TEXT --authority owner|chief_pm --category CATEGORY --urgency URGENCY
                  --recommendation TEXT --requested-by ACTOR [--lane SLUG] [--evidence JSON] [--options JSON]
  decision list [--status STATUS] [--authority AUTHORITY] [--urgency URGENCY] [--lane SLUG]
  decision show <id>
  decision answer <id> --status approved|rejected --by ACTOR --answer TEXT_OR_JSON [--rationale TEXT]
  decision supersede <id> --by-decision ID --by ACTOR [--rationale TEXT]
  decision verify <id>
  lane create --slug SLUG --name NAME [--priority N] [--owner-agent ID]
  lane list
  lane show <slug>
  goal set <slug> --statement TEXT --metric NAME --target N [--current N] [--unit TEXT]
                  [--week YYYY-MM-DD] [--plan JSON] [--approved-decision ID]
  policy list
  friction add --kind KIND --summary TEXT [--lane SLUG] [--rule CODE]
  friction list [--status STATUS]
  auth t3 <host-id> --pairing-code <code> [--hosts-config <path>] [--tokens-dir <path>]
  status
  digest [--window 01:00Z|13:00Z|weekly]
  dashboard [--socket <path>] [--port 0]
  serve [--socket <path>] [--db <path>] [--hosts-config <path>] [--tokens-dir <path>]

Decision categories: ${decisionCategories.join(", ")}`;

type Call = (method: "GET" | "POST" | "PUT", path: string, body?: unknown) => Promise<unknown>;
type Result = { data: unknown; exitCode?: number; text?: string };

interface CliIO {
  stdout?: { write(text: string): unknown };
  stderr?: { write(text: string): unknown };
  env?: NodeJS.ProcessEnv;
}

class UsageError extends Error {}

function parse(argv: readonly string[]): { args: string[]; flags: Map<string, string>; json: boolean } {
  const args: string[] = [];
  const flags = new Map<string, string>();
  let json = false;
  for (let index = 0; index < argv.length; index++) {
    const arg = argv[index]!;
    if (arg === "--json") {
      json = true;
    } else if (arg.startsWith("--")) {
      const equals = arg.indexOf("=");
      const name = equals < 0 ? arg : arg.slice(0, equals);
      const value = equals < 0 ? (name === "--browser" ? "" : argv[++index]) : arg.slice(equals + 1);
      if (name === "--browser" && equals >= 0) throw new UsageError("--browser takes no value");
      if (value === undefined || (equals < 0 && value.startsWith("--"))) {
        throw new UsageError(`${name} requires a value`);
      }
      if (flags.has(name)) throw new UsageError(`duplicate option ${name}`);
      flags.set(name, value);
    } else {
      args.push(arg);
    }
  }
  return { args, flags, json };
}

const commands = new Map<string, { operands: number; flags: string }>([
  ["decision create", { operands: 0, flags: "title authority category urgency recommendation requested-by lane evidence options" }],
  ["decision list", { operands: 0, flags: "status authority urgency lane" }],
  ["decision show", { operands: 1, flags: "" }],
  ["decision answer", { operands: 1, flags: "status by answer rationale" }],
  ["decision supersede", { operands: 1, flags: "by-decision by rationale" }],
  ["decision verify", { operands: 1, flags: "" }],
  ["lane create", { operands: 0, flags: "slug name priority owner-agent" }],
  ["lane list", { operands: 0, flags: "" }],
  ["lane show", { operands: 1, flags: "" }],
  ["goal set", { operands: 1, flags: "statement metric target current unit week plan approved-decision" }],
  ["policy list", { operands: 0, flags: "" }],
  ["friction add", { operands: 0, flags: "kind summary lane rule" }],
  ["friction list", { operands: 0, flags: "status" }],
  ["auth t3", { operands: 1, flags: "pairing-code browser hosts-config tokens-dir" }],
  ["status", { operands: 0, flags: "" }],
  ["digest", { operands: 0, flags: "window" }],
  ["dashboard", { operands: 0, flags: "port" }],
  ["serve", { operands: 0, flags: "hosts-config tokens-dir" }],
]);

function validateCommand(args: string[], flags: Map<string, string>): string {
  const first = args[0] ?? "";
  const single = commands.has(first);
  const command = single ? first : args.slice(0, 2).join(" ");
  const schema = commands.get(command);
  const operands = args.slice(single ? 1 : 2);
  if (schema === undefined || operands.length !== schema.operands || operands.some((arg) => arg.trim() === "")) {
    throw new UsageError("invalid command or operands");
  }
  for (const flag of flags.keys()) {
    if (!schema.flags.split(" ").includes(flag.slice(2))) throw new UsageError(`unknown option ${flag}`);
  }
  return command;
}

function parseJson(value: string | undefined, name: string): unknown {
  if (value === undefined) return undefined;
  try {
    return JSON.parse(value) as unknown;
  } catch {
    throw new UsageError(`--${name} requires valid JSON`);
  }
}

function parseAnswer(value: string): unknown {
  try {
    return JSON.parse(value) as unknown;
  } catch {
    return value;
  }
}

function parseNumber(value: string | undefined, name: string): number | undefined {
  if (value === undefined) return undefined;
  const numeric = Number(value);
  if (value.trim() === "" || !Number.isFinite(numeric)) throw new UsageError(`--${name} requires a number`);
  return numeric;
}

function withQuery(path: string, values: Record<string, string | undefined>): string {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(values)) {
    if (value !== undefined) query.set(key, value);
  }
  return query.size === 0 ? path : `${path}?${query}`;
}

async function execute(call: Call, command: string, operand: string, flags: Map<string, string>): Promise<Result> {
  const optional = (name: string) => flags.get(`--${name}`);
  const required = (name: string) => {
    const value = optional(name);
    if (value === undefined || value.trim() === "") throw new UsageError(`--${name} is required`);
    return value;
  };
  const laneId = async () => {
    const slug = optional("lane");
    if (slug === undefined) return undefined;
    const lane = await call("GET", `/v1/lanes/${encodeURIComponent(slug)}`) as Lane;
    return lane.id;
  };
  const id = encodeURIComponent(operand);
  switch (command) {
    case "status":
      return { data: await call("GET", "/v1/health/summary") };
    case "digest": {
      const window = optional("window") ?? "01:00Z";
      if (!isDigestWindow(window)) throw new UsageError("--window must be 01:00Z, 13:00Z or weekly");
      const digest = await call("GET", withQuery("/v1/digest", { window })) as { markdown: string };
      return { data: digest, text: digest.markdown };
    }
    case "decision create":
      return { data: await call("POST", "/v1/decisions", {
        title: required("title"), authority: required("authority"), category: required("category"),
        urgency: required("urgency"), recommendation: required("recommendation"), requestedBy: required("requested-by"),
        evidence: parseJson(optional("evidence"), "evidence"), options: parseJson(optional("options"), "options"),
        laneId: await laneId(),
      }) };
    case "decision list":
      return { data: await call("GET", withQuery("/v1/decisions", {
        status: optional("status"), authority: optional("authority"), urgency: optional("urgency"), laneId: await laneId(),
      })) };
    case "decision show":
      return { data: await call("GET", `/v1/decisions/${id}`) };
    case "decision answer":
      return { data: await call("POST", `/v1/decisions/${id}/answer`, {
        status: required("status"), decidedBy: required("by"), answer: parseAnswer(required("answer")),
        rationale: optional("rationale"),
      }) };
    case "decision supersede":
      return { data: await call("POST", `/v1/decisions/${id}/supersede`, {
        supersededById: required("by-decision"), actor: required("by"), rationale: optional("rationale"),
      }) };
    case "decision verify": {
      const result = await call("GET", `/v1/decisions/${id}/verify`) as DecisionVerification;
      return { data: result, exitCode: result.valid ? 0 : 1, text: result.reason };
    }
    case "lane create":
      return { data: await call("POST", "/v1/lanes", {
        slug: required("slug"), name: required("name"), priorityRank: parseNumber(optional("priority"), "priority"),
        ownerAgentId: optional("owner-agent"),
      }) };
    case "lane list":
      return { data: await call("GET", "/v1/lanes") };
    case "lane show":
      return { data: await call("GET", `/v1/lanes/${id}`) };
    case "goal set":
      return { data: await call("PUT", `/v1/lanes/${id}/goal`, {
        statement: required("statement"), metricName: required("metric"), metricTarget: parseNumber(required("target"), "target"),
        metricCurrent: parseNumber(optional("current"), "current"), metricUnit: optional("unit"), weekStart: optional("week"),
        weeklyPlan: parseJson(optional("plan"), "plan"), approvedDecisionId: optional("approved-decision"),
      }) };
    case "policy list":
      return { data: await call("GET", "/v1/policy/rules") };
    case "friction add":
      return { data: await call("POST", "/v1/friction", {
        kind: required("kind"), summary: required("summary"), ruleCode: optional("rule"), laneId: await laneId(),
      }) };
    case "friction list":
      return { data: await call("GET", withQuery("/v1/friction", { status: optional("status") })) };
    default:
      throw new UsageError("invalid command");
  }
}

async function waitForShutdown(start: () => void, tick?: () => void): Promise<void> {
  const stop = Promise.withResolvers<void>();
  const onSignal = () => stop.resolve();
  process.once("SIGINT", onSignal);
  process.once("SIGTERM", onSignal);
  let timer: ReturnType<typeof setInterval> | undefined;
  try {
    start();
    if (tick !== undefined) {
      tick();
      timer = setInterval(() => {
        try {
          tick();
        } catch (error) {
          stop.reject(error);
        }
      }, 30_000);
    }
    await stop.promise;
  } finally {
    clearInterval(timer);
    process.off("SIGINT", onSignal);
    process.off("SIGTERM", onSignal);
  }
}

async function serve(dbPath: string, socketPath: string, hosts: readonly PlatformT3Host[] | undefined,
  tokensDir: string, started: () => void): Promise<void> {
  const store = PlatformStore.open(dbPath);
  try {
    const server = await startPlatformApiServer({ store, socketPath });
    let supervisor: T3HostSupervisor | undefined;
    try {
      if (hosts !== undefined) supervisor = new T3HostSupervisor(store, hosts, { tokensDir });
      await waitForShutdown(started, () => { evaluatePolicyAndHealth(store, { now: new Date() }); });
    } finally {
      try {
        await supervisor?.close();
      } finally {
        await server.close();
      }
    }
  } finally {
    store.close();
  }
}

async function authorizeT3(host: PlatformT3Host, pairingCode: string, tokensDir: string) {
  const { authorizationServer, protectedResource } = await discoverOAuthMetadata(host.mcpUrl);
  const callback = await waitForLoopbackCallback();
  const pkce = generatePkcePair();
  try {
    const registration = await registerOAuthClient(authorizationServer.registration_endpoint, {
      clientName: "Ostrom Platform (read-only)", redirectUris: [callback.redirectUri],
    });
    const auth = { clientId: registration.client_id, redirectUri: callback.redirectUri,
      challenge: pkce.challenge, state: pkce.state, resource: protectedResource.resource };
    const code = callback.waitForResult({ state: pkce.state, issuer: authorizationServer.issuer });
    // Keep failures before a redirect from leaving an unhandled callback rejection.
    void code.catch(() => undefined);
    const { redirectTo } = await approveWithPairingCode(authorizationServer.issuer, {
      authorization: buildAuthorizationRequest(auth), code: pairingCode,
    });
    const redirect = new URL(redirectTo);
    const expected = new URL(callback.redirectUri);
    if (redirect.origin !== expected.origin || redirect.pathname !== expected.pathname
      || redirect.username || redirect.password) throw new Error("T3 returned an unexpected callback");
    const response = await fetch(redirect, { redirect: "error", signal: AbortSignal.timeout(5_000) });
    await response.body?.cancel();
    if (!response.ok) throw new Error("T3 OAuth callback failed");
    const token = await exchangeAuthorizationCode(authorizationServer.token_endpoint, {
      clientId: auth.clientId, redirectUri: auth.redirectUri, verifier: pkce.verifier,
      resource: auth.resource, code: (await code).code,
    });
    await saveToken(host.id, token, tokensDir);
    return { hostId: host.id, access: "read-only", expiresAt: new Date(token.expiresAt).toISOString() };
  } finally {
    callback.close();
  }
}

export async function runPlatformCli(argv: readonly string[], io: CliIO = {}): Promise<number> {
  const { stdout = process.stdout, stderr = process.stderr, env = process.env } = io;
  if (argv.length === 1 && argv[0] === "--help") {
    stdout.write(`${usage}\n`);
    return 0;
  }
  let json = false;
  try {
    const parsed = parse(argv);
    json = parsed.json;
    const { args, flags } = parsed;
    const db = flags.get("--db");
    const socket = flags.get("--socket");
    flags.delete("--db");
    flags.delete("--socket");
    const command = validateCommand(args, flags);
    if (db?.trim() === "" || socket?.trim() === "") throw new UsageError("database and socket paths must be non-empty");

    const hostsConfig = flags.get("--hosts-config");
    const explicitTokensDir = flags.get("--tokens-dir");
    if (hostsConfig?.trim() === "" || explicitTokensDir?.trim() === "") throw new UsageError("host config and token paths must be non-empty");

    if (command === "serve") {
      const path = socket ?? defaultPlatformSocketPath(env);
      const hosts = hostsConfig === undefined ? undefined : await loadPlatformHosts(hostsConfig);
      await serve(db ?? defaultPlatformDbPath(env), path, hosts, explicitTokensDir ?? defaultT3TokenDirectory(), () => {
        stdout.write(json ? `${JSON.stringify({ socketPath: path })}\n` : `Listening on ${path}\n`);
      });
      return 0;
    }

    if (command === "auth t3") {
      if (db !== undefined || socket !== undefined) throw new UsageError("auth t3 does not use --db or --socket");
      const pairingCode = flags.get("--pairing-code");
      if (flags.has("--browser") || !pairingCode?.trim()) {
        throw new UsageError("T3 v0.1.13 requires --pairing-code <code> to enforce read-only access");
      }
      const hosts = await loadPlatformHosts(hostsConfig ?? defaultHostsConfigPath(env));
      const host = hosts.find((entry) => entry.id === args[2]);
      if (host === undefined) throw new UsageError("unknown T3 host ID");
      const result = await authorizeT3(host, pairingCode, explicitTokensDir ?? defaultT3TokenDirectory());
      stdout.write(json ? `${JSON.stringify(result)}\n`
        : `Saved read-only authorization for ${result.hostId}. Expires ${result.expiresAt}.\n`);
      return 0;
    }

    if (command === "dashboard") {
      if (db !== undefined) throw new UsageError("dashboard requires a Unix socket");
      const port = parseNumber(flags.get("--port"), "port") ?? 0;
      if (!Number.isInteger(port) || port < 0 || port > 65535) throw new UsageError("--port must be an integer from 0 to 65535");
      const dashboard = await startPlatformDashboard({ socketPath: socket ?? defaultPlatformSocketPath(env), port });
      try {
        await waitForShutdown(() => {
          stdout.write(json ? `${JSON.stringify({ url: dashboard.url })}\n` : `${dashboard.url}\n`);
        });
      } finally {
        await dashboard.close();
      }
      return 0;
    }

    if (db !== undefined && socket !== undefined) throw new UsageError("choose --db or --socket");
    const store = db === undefined ? undefined : PlatformStore.open(db);
    try {
      const client = new PlatformApiClient(socket ?? defaultPlatformSocketPath(env));
      const call: Call = store === undefined
        ? (method, path, data) => client.request(method, path, data)
        : async (method, path, data) => (await dispatchPlatformRequest(store, method, path, async () => data)).body;
      const result = await execute(call, command, args[2] ?? "", flags);
      stdout.write(`${json ? JSON.stringify(result.data) : result.text ?? inspect(result.data, { depth: null, colors: false })}\n`);
      return result.exitCode ?? 0;
    } finally {
      store?.close();
    }
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    stderr.write(json ? `${JSON.stringify({ error: message })}\n` : `ostrom-platform: ${message}\n`);
    return error instanceof UsageError ? 2 : 1;
  }
}

export async function main(argv: readonly string[] = process.argv.slice(2)): Promise<number> {
  return runPlatformCli(argv);
}

if (import.meta.main) process.exitCode = await main();
