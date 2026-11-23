import assert from "node:assert/strict";
import { afterEach, test } from "node:test";
import { Store } from "../store/store.ts";
import type { Runner, SessionOptions } from "../session/runner.ts";
import { Workers } from "../worker/workers.ts";
import { allowedModels, chooseEffort, overrideSelection, recordModelChoice, sessionModel } from "./selection.ts";

const stores: Store[] = [];
afterEach(() => { for (const store of stores.splice(0)) store.close(); });
const config = { repo: "/repo", base: "main", worktrees: "/wt", harness: "codex" as const, model: "legacy", machine: "test" };
function fixture() {
  const store = Store.open(":memory:"); stores.push(store);
  const source = store.ensureSource("github", "owner/repo");
  const thread = store.ensureThread({ sourceId: source.id, kind: "issue", number: 1, url: "https://example.test/issues/1" });
  const trigger = store.insertTrigger({ sourceId: source.id, sourceEventId: "one", threadId: thread, kind: "issue_comment", actor: "operator", url: "https://example.test/issues/1#issuecomment-1", occurredAt: new Date() }).id;
  const ticket = store.recordDecision({ threadId: thread, triggerIds: [trigger], verdict: "accept", reason: "Fix a typo" }).ticketId!;
  store.models.discovered("codex", ["cheap", "large"].map((id) => ({ harness: "codex", id, displayName: id, description: "", efforts: ["low", "medium", "high"], defaultEffort: "medium" })));
  for (const model of ["cheap", "large"]) store.models.change({ action: "allow", harness: "codex", model });
  store.models.change({ action: "set", harness: "codex", model: "large", cost: 2, approval: true });
  return { store, ticket };
}
function assignment(store: Store, ticket: number) {
  const id = store.createAssignment(ticket);
  store.configureNativeAssignment(id, { harness: "codex", model: "cheap", worktree: "/wt", branch: "task", machine: "test" });
  store.startNativeAssignment(id, "session");
  return id;
}
function router(reply: string, calls: string[] = []): Runner {
  const start = (_options: SessionOptions) => ({ send: async (prompt: string) => {
    calls.push(prompt);
    return { sessionId: "router", status: "completed" as const, text: reply, model: null, usage: null };
  } });
  return { start, resume: start };
}

test("model validation records missing/invalid/approval fallbacks and skips retired models", () => {
  const { store, ticket } = fixture();
  for (const choice of [undefined, "outside", "codex/large"]) {
    recordModelChoice(store, ticket, choice, "reason", { choice });
    assert.equal(store.selections.current(ticket, "model")!.value, "codex/cheap");
  }
  assert.match(store.selections.current(ticket, "model")!.reason, /Approval required/);
  store.models.discovered("codex", []);
  assert.deepEqual(allowedModels(store), []);
  assert.throws(() => sessionModel(store, ticket, config), /No allowed model/);
});

test("all-approval selections wait until an explicit override approves the ticket", () => {
  const { store, ticket } = fixture();
  store.models.change({ action: "set", harness: "codex", model: "cheap", approval: true });
  recordModelChoice(store, ticket, "codex/large", "Large task", {});
  assert.throws(() => sessionModel(store, ticket, config), /No allowed model/);
  assert.match(overrideSelection(store, ticket, "model", "codex/large"), /session starts/);
  assert.equal(sessionModel(store, ticket, config).model, "large");
});

test("model overrides preserve an active session and effort validates against that session", () => {
  const { store, ticket } = fixture();
  const id = assignment(store, ticket);
  assert.match(overrideSelection(store, ticket, "model", "codex/large"), /next session/);
  assert.equal(store.getAssignment(id).model, "cheap");
  store.models.change({ action: "set", harness: "codex", model: "large", efforts: ["high"] });
  assert.match(overrideSelection(store, ticket, "effort", "low"), /next turn/);
  assert.throws(() => overrideSelection(store, ticket, "effort", "max"), /effort must be allowed/);
  assert.throws(() => overrideSelection(store, ticket, "model", "unlisted"), /currently allowed/);
});

test("effort choices validate the subset, record inputs/reasons, and durable overrides skip inference", async () => {
  const { store, ticket } = fixture();
  const id = assignment(store, ticket);
  store.models.change({ action: "set", harness: "codex", model: "cheap", efforts: ["low", "medium"] });
  const calls: string[] = [];
  const runner = router('{"effort":"high","reason":"Not allowed"}', calls);
  const options = { store, assignmentId: id, kind: "conflict", prompt: "Resolve the conflict", router: { harness: "codex" as const, model: "router" }, runners: { codex: runner, claude: runner }, signal: new AbortController().signal, threadContext: { kind: "issue" as const, number: 1, url: "https://example.test/issues/1", title: "Issue title", bodyExcerpt: "Bounded body", bodyTruncated: false } };
  const fallback = await chooseEffort(options);
  assert.equal(fallback.effort, "medium");
  assert.match(fallback.reason, /invalid effort answer/);
  assert.match(store.selections.history(ticket).at(-1)!.input, /Resolve the conflict/);
  assert.match(store.selections.history(ticket).at(-1)!.input, /Issue title/);
  assert.match(calls[0]!, /Bounded body/);
  overrideSelection(store, ticket, "effort", "low");
  assert.equal((await chooseEffort(options)).effort, "low");
  assert.equal(calls.length, 1);
});

test("every native send receives chosen effort and model remains fixed across an override", async () => {
  const { store, ticket } = fixture();
  recordModelChoice(store, ticket, "codex/cheap", "Cheapest suitable model", {});
  const sent: { model: string; effort: unknown }[] = [];
  let started = false;
  const worker: Runner = {
    start: (options) => ({ send: async (_prompt, _signal, effort) => {
      if (!started) options.onSessionId?.("worker");
      started = true; sent.push({ model: options.model, effort });
      return { sessionId: "worker", status: "completed", text: "done", model: "cheap", usage: null };
    } }),
    resume: () => { throw new Error("unexpected resume"); },
  };
  const choices: string[] = [];
  const chooser = router('{"effort":"medium","reason":"Moderate first work"}', choices);
  const workers = new Workers({ store, config, router: { harness: "claude", model: "router" }, runners: { codex: worker, claude: chooser }, addWorktree: async () => "task" });
  const dispatched = await workers.dispatch(ticket); await workers.drain();
  overrideSelection(store, ticket, "model", "codex/large");
  overrideSelection(store, ticket, "effort", "low");
  for (const kind of ["review", "conflict", "recovery", "control"] as const) workers.deliver(dispatched.assignmentId, kind, "continue", () => {}, undefined, kind);
  await workers.drain();
  assert.deepEqual(sent, [{ model: "cheap", effort: "medium" }, ...Array.from({ length: 4 }, () => ({ model: "cheap", effort: "low" }))]);
  assert.equal(choices.length, 1);
  assert.ok(store.ticketTurns(ticket).every((turn) => turn.reason && turn.requestedModel === "cheap"));
});
