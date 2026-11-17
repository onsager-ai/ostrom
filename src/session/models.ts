import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { createInterface } from "node:readline";
import { query, type ModelInfo, type SDKUserMessage } from "@anthropic-ai/claude-agent-sdk";
import { sessionConfig } from "./config.ts";

export type Harness = "claude" | "codex";
export interface DiscoveredModel {
  harness: Harness;
  id: string;
  displayName: string;
  description: string;
  efforts: string[];
  defaultEffort: string | null;
}

export function claudeModels(models: ModelInfo[]): DiscoveredModel[] {
  return models.map((model) => ({ harness: "claude", id: model.value, displayName: model.displayName,
    description: model.description, efforts: model.supportedEffortLevels ?? [], defaultEffort: null }));
}

/** Resolve the CLI pinned transitively by the SDK, never the operator's PATH. */
export function codexCli(): string {
  const require = createRequire(import.meta.resolve("@openai/codex-sdk"));
  return join(dirname(require.resolve("@openai/codex/package.json")), "bin", "codex.js");
}

export interface ModelListParams { cursor?: string; limit: number; includeHidden: boolean }
export interface ModelListResponse {
  data: { id: string; displayName: string; description: string; supportedReasoningEfforts: { reasoningEffort: string; description: string }[]; defaultReasoningEffort: string; isDefault: boolean }[];
  nextCursor?: string | null;
}

export async function codexModels(list: (params: ModelListParams) => Promise<unknown>): Promise<DiscoveredModel[]> {
  const models: DiscoveredModel[] = [];
  const cursors = new Set<string>();
  let cursor: string | undefined;
  do {
    const page = await list({ limit: 100, includeHidden: false, ...(cursor === undefined ? {} : { cursor }) }) as ModelListResponse;
    if (!page || !Array.isArray(page.data) || (page.nextCursor != null && typeof page.nextCursor !== "string")) throw new Error("Invalid Codex model/list response");
    for (const model of page.data) {
      if (!model || typeof model.id !== "string" || !model.id || typeof model.displayName !== "string" || typeof model.description !== "string" ||
        typeof model.defaultReasoningEffort !== "string" || typeof model.isDefault !== "boolean" || !Array.isArray(model.supportedReasoningEfforts) ||
        model.supportedReasoningEfforts.some((effort) => !effort || typeof effort.reasoningEffort !== "string" || !effort.reasoningEffort || typeof effort.description !== "string")) {
        throw new Error("Invalid Codex model/list model");
      }
      models.push({ harness: "codex", id: model.id, displayName: model.displayName, description: model.description,
        efforts: model.supportedReasoningEfforts.map((effort) => effort.reasoningEffort), defaultEffort: model.defaultReasoningEffort });
    }
    cursor = page.nextCursor ?? undefined;
    if (cursor !== undefined && cursors.has(cursor)) throw new Error("Codex model/list repeated a cursor");
    if (cursor !== undefined) cursors.add(cursor);
  } while (cursor !== undefined);
  return models;
}

/** Discovery uses the same login/provider environment as native Worker sessions, without a turn. */
export async function discoverModels(harness: Harness, env: Record<string, string | undefined>, signal?: AbortSignal): Promise<DiscoveredModel[]> {
  const config = sessionConfig(env);
  if (harness === "claude") {
    const abortController = new AbortController();
    const abort = () => abortController.abort();
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
    const timer = setTimeout(abort, 30_000);
    // An open, empty stream initializes the control channel without sending a prompt.
    let release!: () => void;
    const pending = new Promise<void>((resolve) => { release = resolve; });
    async function* prompts(): AsyncGenerator<SDKUserMessage> { await pending; }
    const session = query({ prompt: prompts(), options: { env: config.claudeEnv, abortController, persistSession: false } });
    try { return claudeModels(await session.supportedModels()); }
    finally { clearTimeout(timer); signal?.removeEventListener("abort", abort); release(); session.close(); }
  }
  const provider = env["OSTROM_CODEX_PROVIDER"]?.trim();
  const child = spawn(process.execPath, [codexCli(), ...(provider ? ["-c", `model_provider=${JSON.stringify(provider)}`] : []), "app-server"], {
    env: config.codex.env, stdio: ["pipe", "pipe", "ignore"],
  });
  let sequence = 0;
  const waiting = new Map<number, { resolve: (value: unknown) => void; reject: (error: Error) => void }>();
  let failure: Error | undefined;
  const fail = (error: Error) => { failure = error; for (const item of waiting.values()) item.reject(error); waiting.clear(); };
  const lines = createInterface({ input: child.stdout });
  lines.on("line", (line) => {
    try {
      const message = JSON.parse(line) as { id?: number; result?: unknown; error?: unknown };
      if (message.id === undefined) return;
      const item = waiting.get(message.id);
      if (item === undefined) return;
      waiting.delete(message.id);
      if (message.error !== undefined) item.reject(new Error("Codex discovery request refused"));
      else item.resolve(message.result);
    } catch { fail(new Error("Invalid Codex app-server response")); }
  });
  child.on("error", () => fail(new Error("Could not start Codex app-server")));
  child.on("exit", () => fail(new Error("Codex app-server exited during discovery")));
  child.stdin.on("error", () => fail(new Error("Codex app-server input closed")));
  const abort = () => { fail(new Error("Codex discovery cancelled")); child.kill(); };
  signal?.addEventListener("abort", abort, { once: true });
  if (signal?.aborted) abort();
  const timer = setTimeout(() => { fail(new Error("Codex discovery timed out")); child.kill(); }, 30_000);
  const send = (method: string, params: unknown): Promise<unknown> => new Promise((resolve, reject) => {
    if (failure !== undefined) { reject(failure); return; }
    const id = ++sequence;
    waiting.set(id, { resolve, reject });
    child.stdin.write(`${JSON.stringify({ id, method, params })}\n`);
  });
  try {
    await send("initialize", { clientInfo: { name: "ostrom", version: "0.0.0" }, capabilities: { experimentalApi: true, explicitGatewayOauth: true } });
    child.stdin.write(`${JSON.stringify({ method: "initialized" })}\n`);
    return await codexModels((params) => send("model/list", params));
  } finally { clearTimeout(timer); signal?.removeEventListener("abort", abort); lines.close(); child.kill(); }
}
