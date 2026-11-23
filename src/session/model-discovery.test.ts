import assert from "node:assert/strict";
import { test } from "node:test";
import { Store } from "../store/store.ts";
import { modelDiscovery } from "./model-discovery.ts";

test("refresh failures are per harness, preserve cache, and concurrent refreshes share one operation", async () => {
  const store = Store.open(":memory:");
  const messages: string[] = [];
  store.models.discovered("codex", [{ harness: "codex", id: "previous", displayName: "Previous", description: "", efforts: [], defaultEffort: null }]);
  let calls = 0;
  const discovery = modelDiscovery(store, {}, (message) => messages.push(message), async (harness) => {
    calls++;
    if (harness === "codex") throw new Error("offline");
    return [{ harness, id: "sonnet", displayName: "Sonnet", description: "", efforts: ["high"], defaultEffort: null }];
  });
  try {
    const first = discovery.refresh();
    const second = discovery.refresh();
    assert.equal(first, second);
    const view = await first;
    assert.equal(calls, 2);
    assert.deepEqual(view.models.map((row) => row.id), ["sonnet", "previous"]);
    assert.equal(view.discoveries.find((row) => row.harness === "codex")?.lastError, "offline");
    assert.deepEqual(messages, ["codex model discovery failed: offline"]);
  } finally { await discovery.close(); store.close(); }
});

test("closing discovery cancels outstanding requests before the store is closed", async () => {
  const store = Store.open(":memory:");
  let aborted = 0;
  const discovery = modelDiscovery(store, {}, () => {}, async (_harness, _env, signal) => {
    await new Promise<void>((resolve) => signal!.addEventListener("abort", () => { aborted++; resolve(); }, { once: true }));
    throw new Error("aborted");
  });
  void discovery.refresh();
  await discovery.close();
  assert.equal(aborted, 2);
  assert.ok(store.models.view().discoveries.every((row) => row.lastError === null));
  store.close();
});
