import { strict as assert } from "node:assert";
import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { promisify } from "node:util";
import type { ModelInfo } from "@anthropic-ai/claude-agent-sdk";
import { claudeModels, codexCli, codexModels, type ModelListResponse } from "./models.ts";

test("Claude discovery agrees with the pinned SDK's ModelInfo", () => {
  const model: Pick<ModelInfo, "value" | "displayName" | "description" | "supportedEffortLevels"> = {
    value: "sonnet", displayName: "Sonnet", description: "A model", supportedEffortLevels: ["low", "high", "max"],
  };
  assert.deepEqual(claudeModels([model]), [{ harness: "claude", id: "sonnet", displayName: "Sonnet", description: "A model", efforts: ["low", "high", "max"], defaultEffort: null }]);
  assert.deepEqual(claudeModels([{ value: "alias", displayName: "Alias", description: "" }])[0]?.efforts, []);
});

test("Codex discovery follows pagination and rejects malformed pages", async () => {
  const model: ModelListResponse["data"][number] = { id: "sol", displayName: "Sol", description: "A model", supportedReasoningEfforts: [{ reasoningEffort: "high", description: "High" }], defaultReasoningEffort: "high", isDefault: true };
  const cursors: (string | undefined)[] = [];
  const rows = await codexModels(async (params) => { cursors.push(params.cursor); return { data: [{ ...model, id: params.cursor ?? "first" }], nextCursor: params.cursor ? null : "second" }; });
  assert.deepEqual(cursors, [undefined, "second"]);
  assert.deepEqual(rows.map((row) => row.id), ["first", "second"]);
  assert.equal(rows[0]?.defaultEffort, "high");
  await assert.rejects(codexModels(async () => ({ data: [{}] })), /Invalid Codex/);
  await assert.rejects(codexModels(async () => ({ data: [], nextCursor: "same" })), /repeated a cursor/);
});

test("model/list request and consumed response fields agree with the pinned CLI schema", async () => {
  const root = await mkdtemp(join(tmpdir(), "ostrom-model-schema-"));
  try {
    await promisify(execFile)(process.execPath, [codexCli(), "app-server", "generate-json-schema", "--out", root], { timeout: 30_000 });
    const requests = JSON.parse(await readFile(join(root, "ClientRequest.json"), "utf8"));
    const initialize = requests.oneOf.find((request: { properties: { method: { enum: string[] } } }) => request.properties.method.enum.includes("initialize"));
    assert.equal(initialize.properties.params.$ref, "#/definitions/InitializeParams");
    assert.equal(requests.definitions.InitializeParams.properties.clientInfo.$ref, "#/definitions/ClientInfo");
    for (const key of ["name", "version"]) assert.equal(requests.definitions.ClientInfo.properties[key].type, "string");
    for (const key of ["experimentalApi", "explicitGatewayOauth"]) assert.equal(requests.definitions.InitializeCapabilities.properties[key].type, "boolean");
    const params = JSON.parse(await readFile(join(root, "v2/ModelListParams.json"), "utf8"));
    assert.deepEqual(params.properties.cursor.type, ["string", "null"]);
    assert.deepEqual(params.properties.limit.type, ["integer", "null"]);
    assert.deepEqual(params.properties.includeHidden.type, ["boolean", "null"]);
    const response = JSON.parse(await readFile(join(root, "v2/ModelListResponse.json"), "utf8"));
    assert.equal(response.properties.data.items.$ref, "#/definitions/Model");
    assert.ok(response.required.includes("data"));
    assert.deepEqual(response.properties.nextCursor.type, ["string", "null"]);
    const model = response.definitions.Model;
    for (const key of ["id", "displayName", "description"]) {
      assert.equal(model.properties[key].type, "string");
      assert.ok(model.required.includes(key));
    }
    assert.equal(model.properties.isDefault.type, "boolean");
    assert.ok(model.required.includes("isDefault"));
    assert.equal(model.properties.defaultReasoningEffort.$ref, "#/definitions/ReasoningEffort");
    assert.ok(model.required.includes("defaultReasoningEffort"));
    assert.equal(model.properties.supportedReasoningEfforts.items.$ref, "#/definitions/ReasoningEffortOption");
    assert.ok(model.required.includes("supportedReasoningEfforts"));
    assert.equal(response.definitions.ReasoningEffort.type, "string");
    const effort = response.definitions.ReasoningEffortOption;
    assert.equal(effort.properties.reasoningEffort.$ref, "#/definitions/ReasoningEffort");
    assert.equal(effort.properties.description.type, "string");
    assert.deepEqual(effort.required, ["description", "reasoningEffort"]);
  } finally { await rm(root, { recursive: true, force: true }); }
});
