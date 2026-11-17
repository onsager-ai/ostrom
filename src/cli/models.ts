import { readFile } from "node:fs/promises";
import type { ModelChange, ModelView } from "../store/models.ts";
import { Store } from "../store/store.ts";

export const MODELS_USAGE = `usage: ostrom models [--json]
       ostrom models allow|remove <claude|codex>/<model> [--json]
       ostrom models set <harness>/<model> [--efforts a,b] [--approval on|off] [--cost N] [--note TEXT] [--json]
       ostrom models objective <cost|balanced|quality> [--json]
       ostrom models refresh|export [--json]
       ostrom models import <file> [--json]`;

type ModelsCommand = { action: "list"; json: boolean }
  | { action: "refresh"; json: boolean }
  | { action: "export"; json: boolean }
  | { action: "change"; change: ModelChange; json: boolean }
  | { action: "import"; file: string; json: boolean };

export function parseModels(argv: readonly string[]): ModelsCommand | null {
  const json = argv.at(-1) === "--json";
  const args = json ? argv.slice(0, -1) : argv;
  if (args.length === 0) return { action: "list", json };
  if (args.length === 1 && (args[0] === "refresh" || args[0] === "export")) return { action: args[0], json };
  if (args.length === 2 && args[0] === "import") return { action: "import", file: args[1]!, json };
  if (args.length === 2 && args[0] === "objective" && (args[1] === "cost" || args[1] === "balanced" || args[1] === "quality")) {
    return { action: "change", change: { action: "objective", objective: args[1] }, json };
  }
  const [action, target] = args;
  if (action !== "allow" && action !== "remove" && action !== "set") return null;
  const match = /^(claude|codex)\/(.+)$/.exec(target ?? "");
  if (!match) return null;
  const harness = match[1] as "claude" | "codex";
  const model = match[2]!;
  if (action !== "set") return args.length === 2 ? { action: "change", change: { action, harness, model }, json } : null;
  const change: Extract<ModelChange, { action: "set" }> = { action, harness, model };
  if (args.length < 4 || args.length % 2 !== 0) return null;
  const seen = new Set<string>();
  for (let index = 2; index < args.length; index += 2) {
    const flag = args[index]!;
    const value = args[index + 1]!;
    if (seen.has(flag)) return null;
    seen.add(flag);
    if (flag === "--efforts") change.efforts = value === "" ? [] : value.split(",");
    else if (flag === "--approval" && (value === "on" || value === "off")) change.approval = value === "on";
    else if (flag === "--cost" && value.trim() !== "" && Number.isFinite(Number(value)) && Number(value) > 0) change.cost = Number(value);
    else if (flag === "--note") change.note = value;
    else return null;
  }
  return { action: "change", change, json };
}

type Call = (method: "GET" | "POST", path: string, body?: unknown) => Promise<unknown>;
export async function modelsMain(argv: readonly string[], env: Record<string, string | undefined>, call: Call): Promise<number> {
  const command = parseModels(argv);
  if (!command) { process.stderr.write(`${MODELS_USAGE}\n`); return 2; }
  try {
    let value: unknown;
    try {
      if (command.action === "list") value = await call("GET", "/v1/models");
      else if (command.action === "export") value = await call("GET", "/v1/models/export");
      else if (command.action === "refresh") value = await call("POST", "/v1/models/refresh");
      else if (command.action === "import") value = await call("POST", "/v1/models", { action: "import", value: JSON.parse(await readFile(command.file, "utf8")) });
      else value = await call("POST", "/v1/models", command.change);
    } catch (error) {
      const unavailable = error instanceof Error && ["ECONNREFUSED", "ENOENT", "ENOTSOCK"].some((code) => error.message.includes(code));
      if ((command.action !== "list" && command.action !== "export") || !unavailable) throw error;
      const path = env["OSTROM_DB"]?.trim();
      if (!path) throw new Error("ostromd is not running; set OSTROM_DB for read-only fallback");
      const store = Store.open(path);
      try { value = command.action === "export" ? store.models.export() : store.models.view(); }
      finally { store.close(); }
    }
    if (command.json || command.action === "export") process.stdout.write(`${JSON.stringify(value)}\n`);
    else process.stdout.write(renderModels(value as ModelView));
    return 0;
  } catch (error) {
    process.stderr.write(`ostrom: ${error instanceof Error ? error.message : String(error)}\n`);
    return 1;
  }
}

export function renderModels(view: ModelView): string {
  const lines = [`objective ${view.objective}`];
  for (const discovery of view.discoveries) lines.push(`${discovery.harness} discovered ${discovery.discoveredAt ?? "never"}${discovery.lastError === null ? "" : `; error: ${discovery.lastError}`}`);
  for (const model of view.models) {
    lines.push(`${model.harness}/${model.id}  ${model.allowed ? "allowed" : "not allowed"}  ${model.displayName}`);
    lines.push(`  efforts: ${model.efforts.join(",") || "default only"}; default: ${model.defaultEffort ?? "harness default"}`);
    if (model.selection) lines.push(`  selected efforts: ${model.selection.efforts.join(",") || "default only"}; approval: ${model.selection.approval ? "on" : "off"}; cost: ${model.selection.cost}; note: ${model.selection.note}`);
    if (model.flags.length) lines.push(`  ${model.flags.join("; ")}`);
    if (model.description) lines.push(`  ${model.description}`);
  }
  return `${lines.join("\n")}\n`;
}
