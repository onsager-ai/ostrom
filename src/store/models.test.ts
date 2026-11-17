import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import type { DiscoveredModel } from "../session/models.ts";
import { Store } from "./store.ts";

const model: DiscoveredModel = { harness: "codex", id: "sol", displayName: "Sol", description: "Model", efforts: ["low", "high"], defaultEffort: "high" };

test("catalogs and choices persist; failed discovery retains the last good catalog and timestamp", () => {
  const root = mkdtempSync(join(tmpdir(), "ostrom-models-"));
  const path = join(root, "store.db");
  let store = Store.open(path, { now: () => new Date("2026-11-17T00:00:00Z") });
  try {
    assert.deepEqual(store.models.export(), { version: 1, objective: "cost", selections: [] });
    store.models.discovered("codex", [model]);
    assert.deepEqual(store.models.view().models[0]?.flags, ["not allowed", "newly discovered"]);
    store.models.change({ action: "allow", harness: "codex", model: "sol" });
    store.models.change({ action: "set", harness: "codex", model: "sol", efforts: ["low"], approval: true, cost: 2.5, note: "Rust" });
    store.models.change({ action: "objective", objective: "quality" });
    const before = store.models.export();
    store.models.failed("codex", "offline");
    store.close();
    store = Store.open(path);
    assert.deepEqual(store.models.export(), before);
    assert.equal(store.models.view().models[0]?.allowed, true);
    assert.deepEqual(store.models.view().discoveries.find((row) => row.harness === "codex"), { harness: "codex", discoveredAt: "2026-11-17T00:00:00.000Z", lastError: "offline" });
    store.models.discovered("codex", []);
    assert.deepEqual(store.models.view().models[0]?.flags, ["allowed but no longer offered"]);
    assert.equal(store.models.view().discoveries.find((row) => row.harness === "codex")?.lastError, null);
    assert.throws(() => store.models.change({ action: "set", harness: "codex", model: "sol", note: "retired" }), /not offered/);
    store.models.change({ action: "remove", harness: "codex", model: "sol" });
    assert.equal(store.models.view().models[0]?.allowed, false);
  } finally { store.close(); rmSync(root, { recursive: true, force: true }); }
});

test("all mutations validate model, effort and settings, and import is atomic", () => {
  const store = Store.open(":memory:");
  try {
    store.models.discovered("codex", [model]);
    assert.throws(() => store.models.change({ action: "allow", harness: "codex", model: "unknown" }), /not offered/);
    assert.throws(() => store.models.change({ action: "set", harness: "codex", model: "sol", note: "x" }), /allow the model/);
    store.models.change({ action: "allow", harness: "codex", model: "sol" });
    const before = store.models.export();
    for (const patch of [{ efforts: ["max"] }, { efforts: ["low", "low"] }, { cost: 0 }, { cost: Infinity }, { approval: "on" }, { note: 3 }]) {
      assert.throws(() => store.models.change({ action: "set", harness: "codex", model: "sol", ...patch }));
      assert.deepEqual(store.models.export(), before);
    }
    assert.throws(() => store.models.change({ action: "objective", objective: "cheap" }));
    assert.throws(() => store.models.change({ action: "import", value: { ...before, objective: "quality", selections: [...before.selections, { ...before.selections[0], model: "unknown" }] } }), /not offered/);
    assert.deepEqual(store.models.export(), before);
    const exported = { ...before, objective: "balanced", selections: before.selections.map((row) => ({ ...row, efforts: ["low"], note: "imported" })) };
    store.models.change({ action: "import", value: exported });
    assert.deepEqual(store.models.export(), exported);
    store.models.discovered("codex", [{ ...model, efforts: ["high"] }]);
    assert.deepEqual(store.models.view().models[0]?.flags, ["selected effort no longer offered"]);
  } finally { store.close(); }
});
