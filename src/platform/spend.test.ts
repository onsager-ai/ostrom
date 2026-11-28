import assert from "node:assert/strict";
import { test } from "node:test";

import { PlatformStore } from "./store.ts";
import { SemonCliSpendSource, querySemonTeamRollups, refreshSpendSnapshots } from "./spend.ts";

const factColumns = [
  "session_uid", "input_tokens", "output_tokens", "cache_read_tokens", "cache_write_5m_tokens",
  "cache_write_1h_tokens", "usd_est", "reported_usd", "usage_complete", "coverage", "as_of", "billed_usd",
];
const dimColumns = ["session_uid", "origin_namespace", "native_harness", "native_session_id"];
const schema = (...tables: [string, string[]][]) => ({ tables: tables.map(([name, cols]) =>
  ({ name, columns: cols.map((column) => ({ name: column })) })) });

const stepColumns = ["session_uid", "ts", "usage_owner", "price_status", "usd_est", "price_version"];

test("Semon queries priced exclusive steps over half-open UTC spend windows", async () => {
  const now = "2026-12-03T03:15:45.678+08:00";
  const until = "2026-12-02T19:15:45.678Z";
  const starts = ["2026-12-02T00:00:00.000Z", "2026-11-30T00:00:00.000Z", "2026-12-01T00:00:00.000Z"];
  const sql = `SELECT
  count(DISTINCT session_uid) AS session_count,
  count(*) FILTER (WHERE usage_owner = 'exclusive') AS exclusive_steps,
  count(*) FILTER (WHERE usage_owner = 'exclusive' AND price_status = 'priced') AS priced_steps,
  count(*) FILTER (WHERE usage_owner = 'exclusive' AND price_status = 'unpriced') AS unpriced_steps,
  count(*) FILTER (WHERE usage_owner <> 'exclusive') AS non_exclusive_steps,
  sum(usd_est) FILTER (WHERE usage_owner = 'exclusive' AND price_status = 'priced') AS priced_usd_est
FROM fact_step
WHERE epoch_ms(ts) >= ? AND epoch_ms(ts) < ?`;
  const calls: string[][] = [];
  const rollups = await querySemonTeamRollups("/path/with spaces/semon", now, async (bin, args) => {
    assert.equal(bin, "/path/with spaces/semon");
    calls.push(args);
    if (args[1] === "schema") return JSON.stringify(schema(["fact_step", stepColumns]));
    const index = calls.length - 2;
    assert.deepEqual(args, ["query", "sql", sql, "--params", JSON.stringify([Date.parse(starts[index]!), Date.parse(until)])]);
    return JSON.stringify({ truncated: false, snapshot: `generation-${index}`, coverage: "complete",
      rows: [[2, 4, index === 0 ? 0 : 1, 3, 2, index === 0 ? null : 1.75]] });
  });
  assert.deepEqual(calls[0], ["query", "schema"]);
  assert.equal(calls.length, 4);
  assert.deepEqual(rollups, ["daily", "weekly", "monthly"].map((period, index) => ({
    scopeKind: "team", scopeId: null, period, source: "semon", since: starts[index], until, observedAt: until,
    sessionCount: 2, unpricedCount: 3, incompleteCount: 5, usdEst: index === 0 ? null : 1.75,
    preferredUsd: index === 0 ? null : 1.75, sourceReportedUsd: null, billedUsd: null,
    coverage: "partial", snapshotId: `generation-${index}`, asOf: null,
  })));
});

test("Semon rollups reject an unsupported schema or truncated result", async () => {
  const now = "2026-11-27T13:00:00Z";
  await assert.rejects(querySemonTeamRollups("semon", now, async (_bin, args) => {
    assert.deepEqual(args, ["query", "schema"]);
    return JSON.stringify(schema(["fact_step", stepColumns.slice(0, -1)]));
  }), /unsupported Semon schema/);
  await assert.rejects(querySemonTeamRollups("semon", now, async (_bin, args) => JSON.stringify(args[1] === "schema"
    ? schema(["fact_step", stepColumns])
    : { truncated: true, rows: [[2, 4, 1, 3, 2, 1.75]] })), /invalid Semon query result/);
});

test("Semon validates schema, resolves exact native identity and loads only the session aggregate", async () => {
  const calls: string[][] = [];
  const source = new SemonCliSpendSource("/path/with spaces/semon", async (bin, args) => {
    assert.equal(bin, "/path/with spaces/semon");
    calls.push(args);
    if (args[1] === "schema") return JSON.stringify(schema(["dim_session", dimColumns], ["fact_session", factColumns]));
    assert.equal(args.length, 3);
    assert.equal(args[1], "sql");
    const query = args[2] ?? "";
    if (query.includes("dim_session")) {
      assert.match(query, /WHERE origin_namespace = 'origin-1' AND native_harness = 'codex' AND native_session_id = 'native:1' LIMIT 2/);
      return JSON.stringify({ truncated: false, rows: [["uid-1"]] });
    }
    assert.match(query, /FROM fact_session WHERE session_uid = 'uid-1' LIMIT 2/);
    assert(!query.includes("fact_step"));
    return JSON.stringify({ snapshot: "generation", truncated: false,
      rows: [[2, 3, 5, 7, 11, null, 0, false, 1.25, "partial", "2026-11-26T00:00:00Z"]] });
  });
  const identity = { originNamespace: "origin-1", nativeHarness: "codex", nativeSessionId: "native:1" };
  assert.equal(await source.resolveSessionUid(identity), "uid-1");
  const result = await source.getSnapshot("uid-1");
  assert.equal(result.status, "available");
  assert(result.snapshot);
  const { observedAt, ...snapshot } = result.snapshot;
  assert(Number.isFinite(Date.parse(observedAt)));
  assert.deepEqual(snapshot, {
    sessionUid: "uid-1", source: "semon", inputTokens: 2, outputTokens: 3, cachedTokens: 23,
    usdEst: null, sourceReportedUsd: 0, billedUsd: 1.25, snapshotId: "generation", asOf: "2026-11-26T00:00:00Z",
    coverage: "partial",
  });
  assert.equal(await source.resolveSessionUid({ ...identity, nativeSessionId: "' OR true --" }), null);
  assert.deepEqual(await source.getSnapshot("x'; DELETE"), { status: "invalid_session", snapshot: null });
  assert.deepEqual(calls[0], ["query", "schema"]);
  assert.equal(calls.length, 3);
});

test("Semon gracefully refuses missing or ambiguous identity and unavailable schema or CLI", async () => {
  const missing = new SemonCliSpendSource("semon", async (_bin, args) => {
    assert.deepEqual(args, ["query", "schema"]);
    return JSON.stringify(schema(["dim_session", ["session_uid"]]));
  });
  const identity = { originNamespace: "origin", nativeHarness: "codex", nativeSessionId: "native" };
  assert.equal(await missing.resolveSessionUid(identity), null);
  assert.equal((await missing.getSnapshot("uid")).status, "unavailable");
  const duplicate = new SemonCliSpendSource("semon", async (_bin, args) => JSON.stringify(args[1] === "schema"
    ? schema(["dim_session", dimColumns])
    : { truncated: false, rows: [["uid-1"], ["uid-2"]] }));
  assert.equal(await duplicate.resolveSessionUid(identity), null);
  const absent = new SemonCliSpendSource("missing-cli", async () => { throw new Error("ENOENT"); });
  assert.deepEqual(await absent.getSnapshot("uid"), { status: "unavailable", snapshot: null });
});

test("Semon supports the released fact schema without billed USD or an as-of timestamp", async () => {
  let rows: unknown[][] = [[0, 0, 0, 0, 0, null, null, true, null, null, null]];
  const source = new SemonCliSpendSource("semon", async (_bin, args) => {
    if (args[1] === "schema") return JSON.stringify(schema(["fact_session", factColumns.slice(0, 9)]));
    assert.match(args[2] ?? "", /usage_complete, NULL, NULL, NULL/);
    return JSON.stringify({ snapshot: "generation", truncated: false, rows });
  });
  const { snapshot } = await source.getSnapshot("uid");
  assert.equal(snapshot?.usdEst, null);
  assert.equal(snapshot?.sourceReportedUsd, null);
  assert.equal(snapshot?.billedUsd, null);
  assert.equal(snapshot?.asOf, null);
  assert.equal(snapshot?.cachedTokens, 0);
  assert.equal(snapshot?.coverage, "complete");
  rows = [];
  assert.equal((await source.getSnapshot("uid")).status, "not_found");
});

test("refresh links only exact native identities and skips a bad UID before a valid session", async (t) => {
  const store = PlatformStore.open(":memory:");
  t.after(() => store.close());
  const now = "2026-11-27T13:00:00Z";
  const agent = store.upsertAgent({ id: "worker", role: "worker", hostId: "east", connectorKind: "t3",
    externalRef: "thread", label: "Worker", status: "idle" });
  const run = { agentId: agent.id, hostId: "east", connectorKind: "t3", status: "completed", startedAt: now } as const;
  store.upsertRun({ ...run, id: "a", externalRunRef: "a", semonSessionUid: "unsupported uid" });
  store.upsertRun({ ...run, id: "b", externalRunRef: "b",
    semonIdentity: { originNamespace: "east", nativeHarness: "codex", nativeSessionId: "native-1" } });
  store.upsertRun({ ...run, id: "c", externalRunRef: "native-2", semonSessionUid: "linked" });
  store.upsertRun({ ...run, id: "d", externalRunRef: "native-1" });
  store.upsertSpendSnapshot({ source: "semon", sessionUid: "unsupported uid", usdEst: 9, coverage: "complete" });
  const queries: string[] = [];
  let fail = false;
  let omitIdentitySchema = false;
  let missLinkedSession = false;
  const makeSource = () => new SemonCliSpendSource("semon", async (_bin, args) => {
    if (args[1] === "schema") return JSON.stringify(schema(
      ["dim_session", omitIdentitySchema ? ["session_uid"] : dimColumns], ["fact_session", factColumns]));
    const sql = args[2] ?? "";
    queries.push(sql);
    if (sql.includes("dim_session")) {
      assert.match(sql, /origin_namespace = 'east' AND native_harness = 'codex' AND native_session_id = 'native-1'/);
      return JSON.stringify({ truncated: false, rows: [["exact-uid"]] });
    }
    if (fail) throw new Error("process unavailable");
    if (missLinkedSession && sql.includes("'linked'")) return JSON.stringify({ truncated: false, rows: [] });
    return JSON.stringify({ truncated: false, rows: [[1, 2, 0, 0, 0, 3, null, true, null, null, null]] });
  });
  const signal = new AbortController().signal;
  const source = makeSource();
  const summary = () => store.summarizeSpend({ scopeKind: "team", scopeId: null, period: "daily", source: "semon" }, now);
  await refreshSpendSnapshots(store, source, signal);
  assert.deepEqual(store.listRuns().map((item) => item.semonSessionUid), ["unsupported uid", "exact-uid", "linked", null]);
  assert.equal(store.getSpendSnapshot("linked")?.usdEst, 3);
  assert.equal(store.getSpendSnapshot("exact-uid")?.usdEst, 3);
  assert.equal(summary().staleSnapshotCount, 1);
  assert.equal(queries.length, 3);
  missLinkedSession = true;
  await refreshSpendSnapshots(store, source, signal);
  assert.equal(summary().staleSnapshotCount, 2);
  assert.deepEqual(queries.slice(3).map((sql) => /FROM fact_session WHERE session_uid = '([^']+)'/.exec(sql)?.[1]),
    ["exact-uid", "linked"]);
  fail = true;
  await refreshSpendSnapshots(store, source, signal);
  assert.equal(summary().staleSnapshotCount, 3);
  fail = false;
  missLinkedSession = false;
  omitIdentitySchema = true;
  store.upsertRun({ ...run, id: "e", externalRunRef: "e",
    semonIdentity: { originNamespace: "east", nativeHarness: "codex", nativeSessionId: "native-3" } });
  await refreshSpendSnapshots(store, makeSource(), signal);
  assert.equal(store.listRuns().at(-1)?.semonSessionUid, null);
  assert.equal(summary().staleSnapshotCount, 1);
});
