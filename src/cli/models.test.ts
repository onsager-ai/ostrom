import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { localApi } from "../api/server.ts";
import { Store } from "../store/store.ts";
import { main } from "./main.ts";
import { parseModels, renderModels } from "./models.ts";

test("models parser handles settings, slash-containing IDs, and refuses invalid flags", () => {
  assert.deepEqual(parseModels(["set", "codex/provider/model", "--efforts", "low,high", "--approval", "on", "--cost", "2", "--note", "Rust", "--json"]), {
    action: "change", json: true, change: { action: "set", harness: "codex", model: "provider/model", efforts: ["low", "high"], approval: true, cost: 2, note: "Rust" },
  });
  for (const args of [["allow", "other/model"], ["set", "claude/sonnet"], ["set", "codex/sol", "--cost", "NaN"], ["objective", "cheap"], ["refresh", "extra"]]) assert.equal(parseModels(args), null);
});

test("models CLI exercises API mutations, JSON export/import, refresh refusal and offline reads", async () => {
  const root = mkdtempSync(join(tmpdir(), "ostrom-models-cli-"));
  const env = { XDG_RUNTIME_DIR: root, OSTROM_DB: join(root, "store.db") };
  const store = Store.open(env.OSTROM_DB);
  const original = process.stdout.write;
  let output = "";
  let offline = false;
  const api = localApi(store, {
    socketPath: join(root, "ostrom", "ostromd.sock"),
    control: { pause: async () => {}, resume: async () => {}, stop: async () => {}, extend: async () => {}, preview: async () => "" },
    refreshModels: async () => {
      if (offline) store.models.failed("codex", "offline");
      else store.models.discovered("codex", [{ harness: "codex", id: "sol", displayName: "Sol", description: "Model", efforts: ["low", "high"], defaultEffort: "high" }]);
      return store.models.view();
    },
  });
  try {
    process.stdout.write = ((chunk: string) => { output += chunk; return true; }) as typeof process.stdout.write;
    await api.start();
    assert.equal(await main(["models", "refresh"], env), 0);
    assert.match(output, /newly discovered/);
    assert.equal(await main(["models", "allow", "codex/sol"], env), 0);
    assert.equal(await main(["models", "set", "codex/sol", "--efforts", "low", "--approval", "on", "--cost", "3", "--note", "Rust"], env), 0);
    assert.equal(await main(["models", "set", "codex/sol", "--efforts", "max"], env), 1);
    assert.equal(await main(["models", "allow", "codex/missing"], env), 1);
    assert.equal(await main(["models", "objective", "balanced"], env), 0);
    output = "";
    assert.equal(await main(["models", "export"], env), 0);
    const exported = JSON.parse(output);
    assert.equal(exported.objective, "balanced");
    const file = join(root, "models.json");
    writeFileSync(file, output);
    assert.equal(await main(["models", "remove", "codex/sol"], env), 0);
    assert.equal(await main(["models", "import", file], env), 0);
    assert.deepEqual(store.models.export(), exported);
    offline = true;
    assert.equal(await main(["models", "refresh"], env), 1);
    assert.match(renderModels(store.models.view()), /error: offline/);
    await api.close();
    output = "";
    assert.equal(await main(["models", "--json"], env), 0);
    assert.equal(JSON.parse(output).models[0].allowed, true);
    output = "";
    assert.equal(await main(["models", "export"], env), 0);
    assert.deepEqual(JSON.parse(output), exported);
    assert.equal(await main(["models", "remove", "codex/sol"], env), 1);
  } finally { process.stdout.write = original; await api.close(); store.close(); rmSync(root, { recursive: true, force: true }); }
});
