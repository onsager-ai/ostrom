#!/usr/bin/env node

import { inspect } from "node:util";

import {
  defaultPlatformDbPath, defaultPlatformSocketPath, dispatchPlatformRequest,
  PlatformApiClient, startPlatformApiServer,
} from "./api.ts";
import { PlatformStore } from "./store.ts";
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
  serve [--socket <path>] [--db <path>]

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
      const value = equals < 0 ? argv[++index] : arg.slice(equals + 1);
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
  ["serve", { operands: 0, flags: "" }],
]);

function validateCommand(args: string[], flags: Map<string, string>): string {
  const command = args[0] === "serve" ? "serve" : args.slice(0, 2).join(" ");
  const schema = commands.get(command);
  const operands = command === "serve" ? args.slice(1) : args.slice(2);
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

async function serve(dbPath: string, socketPath: string, started: () => void): Promise<void> {
  const store = PlatformStore.open(dbPath);
  try {
    const server = await startPlatformApiServer({ store, socketPath });
    try {
      await new Promise<void>((resolve) => {
        const stop = () => {
          process.off("SIGINT", stop);
          process.off("SIGTERM", stop);
          resolve();
        };
        process.once("SIGINT", stop);
        process.once("SIGTERM", stop);
        started();
      });
    } finally {
      await server.close();
    }
  } finally {
    store.close();
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

    if (command === "serve") {
      const path = socket ?? defaultPlatformSocketPath(env);
      await serve(db ?? defaultPlatformDbPath(env), path, () => {
        stdout.write(json ? `${JSON.stringify({ socketPath: path })}\n` : `Listening on ${path}\n`);
      });
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
