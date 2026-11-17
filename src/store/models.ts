import type { DatabaseSync } from "node:sqlite";
import type { DiscoveredModel, Harness } from "../session/models.ts";

export type Objective = "cost" | "balanced" | "quality";
export interface ModelSelection {
  harness: Harness;
  model: string;
  allowed: boolean;
  efforts: string[];
  approval: boolean;
  cost: number;
  note: string;
}
export interface ModelExport { version: 1; objective: Objective; selections: ModelSelection[] }
export interface ModelView {
  objective: Objective;
  discoveries: { harness: Harness; discoveredAt: string | null; lastError: string | null }[];
  models: (DiscoveredModel & { selection: ModelSelection | null; allowed: boolean; flags: string[] })[];
}
export type ModelChange =
  | { action: "allow" | "remove"; harness: Harness; model: string }
  | { action: "set"; harness: Harness; model: string; efforts?: string[]; approval?: boolean; cost?: number; note?: string }
  | { action: "objective"; objective: Objective }
  | { action: "import"; value: unknown };

function object(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("expected an object");
  return value as Record<string, unknown>;
}
function harness(value: unknown): Harness {
  if (value !== "claude" && value !== "codex") throw new Error("harness must be claude or codex");
  return value;
}
function objective(value: unknown): Objective {
  if (value !== "cost" && value !== "balanced" && value !== "quality") throw new Error("objective must be cost, balanced or quality");
  return value;
}
function selection(value: unknown): ModelSelection {
  const row = object(value);
  if (typeof row["model"] !== "string" || !row["model"]) throw new Error("model is required");
  if (typeof row["allowed"] !== "boolean" || typeof row["approval"] !== "boolean") throw new Error("allowed and approval must be booleans");
  if (!Array.isArray(row["efforts"]) || row["efforts"].some((effort) => typeof effort !== "string") || new Set(row["efforts"]).size !== row["efforts"].length) throw new Error("efforts must be a list of unique strings");
  if (typeof row["cost"] !== "number" || !Number.isFinite(row["cost"]) || row["cost"] <= 0) throw new Error("cost must be a finite positive number");
  if (typeof row["note"] !== "string") throw new Error("note must be text");
  return { harness: harness(row["harness"]), model: row["model"], allowed: row["allowed"], efforts: row["efforts"] as string[], approval: row["approval"], cost: row["cost"], note: row["note"] };
}

/** Lives on Store's connection; discovery failures never overwrite the last successful catalog. */
export class ModelStore {
  readonly #db: DatabaseSync;
  readonly #now: () => Date;
  constructor(db: DatabaseSync, now: () => Date) { this.#db = db; this.#now = now; }

  discovered(harness: Harness, models: DiscoveredModel[]): void {
    if (models.some((model) => model.harness !== harness) || new Set(models.map((model) => model.id)).size !== models.length) throw new Error("invalid discovered catalog");
    this.#db.prepare("UPDATE model_discovery SET models = ?, discovered_at = ?, last_error = NULL WHERE harness = ?")
      .run(JSON.stringify(models), this.#now().toISOString(), harness);
  }

  failed(harness: Harness, error: string): void {
    this.#db.prepare("UPDATE model_discovery SET last_error = ? WHERE harness = ?").run(error, harness);
  }

  export(): ModelExport {
    const rows = this.#db.prepare("SELECT * FROM model_selection ORDER BY harness, model").all();
    return { version: 1, objective: this.#db.prepare("SELECT objective FROM model_objective WHERE singleton = 1").get()!["objective"] as Objective,
      selections: rows.map((row) => ({ harness: row["harness"] as Harness, model: row["model"] as string, allowed: row["allowed"] === 1,
        efforts: JSON.parse(row["efforts"] as string) as string[], approval: row["approval"] === 1, cost: row["cost"] as number, note: row["note"] as string })) };
  }

  view(): ModelView {
    const settings = this.export();
    const discoveries: ModelView["discoveries"] = [];
    const models: ModelView["models"] = [];
    for (const row of this.#db.prepare("SELECT * FROM model_discovery ORDER BY harness").all()) {
      discoveries.push({ harness: row["harness"] as Harness, discoveredAt: row["discovered_at"] as string | null, lastError: row["last_error"] as string | null });
      for (const model of JSON.parse(row["models"] as string) as DiscoveredModel[]) {
        const selected = settings.selections.find((item) => item.harness === model.harness && item.model === model.id) ?? null;
        const flags = selected?.allowed ? [] : ["not allowed"];
        if (selected === null) flags.push("newly discovered");
        if (selected?.allowed && selected.efforts.some((effort) => !model.efforts.includes(effort))) flags.push("selected effort no longer offered");
        models.push({ ...model, selection: selected, allowed: selected?.allowed ?? false, flags });
      }
    }
    for (const selected of settings.selections) {
      if (models.some((model) => model.harness === selected.harness && model.id === selected.model)) continue;
      models.push({ harness: selected.harness, id: selected.model, displayName: selected.model, description: "", efforts: [], defaultEffort: null,
        selection: selected, allowed: selected.allowed, flags: selected.allowed ? ["allowed but no longer offered"] : ["not allowed", "no longer offered"] });
    }
    return { objective: settings.objective, discoveries, models };
  }

  /** Validate the entire request before mutating; import replaces all selections atomically. */
  change(value: unknown): ModelView {
    const change = object(value);
    const action = change["action"];
    if (action === "objective") {
      this.#db.prepare("UPDATE model_objective SET objective = ? WHERE singleton = 1").run(objective(change["objective"]));
    } else if (action === "import") {
      const input = object(change["value"]);
      if (input["version"] !== 1 || !Array.isArray(input["selections"])) throw new Error("expected model export version 1");
      const goal = objective(input["objective"]);
      const rows = input["selections"].map(selection);
      if (new Set(rows.map((row) => `${row.harness}/${row.model}`)).size !== rows.length) throw new Error("duplicate model selection");
      for (const row of rows) this.#validate(row);
      this.#db.exec("BEGIN IMMEDIATE");
      try {
        this.#db.exec("DELETE FROM model_selection");
        for (const row of rows) this.#write(row);
        this.#db.prepare("UPDATE model_objective SET objective = ? WHERE singleton = 1").run(goal);
        this.#db.exec("COMMIT");
      } catch (error) { this.#db.exec("ROLLBACK"); throw error; }
    } else if (action === "allow" || action === "remove" || action === "set") {
      const kind = harness(change["harness"]);
      const id = change["model"];
      if (typeof id !== "string" || !id) throw new Error("model is required");
      const offered = this.#offered(kind, id);
      const previous = this.export().selections.find((row) => row.harness === kind && row.model === id);
      if (!offered && !(action === "remove" && previous)) throw new Error(`model is not offered: ${kind}/${id}`);
      if (action === "set" && !previous?.allowed) throw new Error("allow the model before setting its options");
      const row = selection({ ...(previous ?? { harness: kind, model: id, efforts: offered!.efforts, approval: false, cost: 1, note: "" }),
        allowed: action === "remove" ? false : true,
        ...(action === "set" ? Object.fromEntries(["efforts", "approval", "cost", "note"].filter((key) => change[key] !== undefined).map((key) => [key, change[key]])) : {}) });
      // A retired selection can always be disabled. All additions and edits need a current catalog.
      if (action !== "remove") this.#validate(row);
      this.#write(row);
    } else throw new Error("unknown model action");
    return this.view();
  }

  #offered(harness: Harness, id: string): DiscoveredModel | undefined {
    const row = this.#db.prepare("SELECT models FROM model_discovery WHERE harness = ?").get(harness)!;
    return (JSON.parse(row["models"] as string) as DiscoveredModel[]).find((model) => model.id === id);
  }
  #validate(row: ModelSelection): void {
    const offered = this.#offered(row.harness, row.model);
    if (!offered) throw new Error(`model is not offered: ${row.harness}/${row.model}`);
    if (row.efforts.some((effort) => !offered.efforts.includes(effort))) throw new Error("efforts must be a subset of the discovered efforts");
  }
  #write(row: ModelSelection): void {
    this.#db.prepare(`INSERT INTO model_selection (harness, model, allowed, efforts, approval, cost, note) VALUES (?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT (harness, model) DO UPDATE SET allowed = excluded.allowed, efforts = excluded.efforts, approval = excluded.approval, cost = excluded.cost, note = excluded.note`)
      .run(row.harness, row.model, Number(row.allowed), JSON.stringify(row.efforts), Number(row.approval), row.cost, row.note);
  }
}
