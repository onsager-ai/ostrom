// The scripted stall (docs/direction.md, milestone 1 pass criterion): seeds a throwaway store,
// creates a native Worker's worktree, and starts it on gpt-6-luna with one question to ask.
// After OSTROM_STALL_MINUTES the Watchdog flags the stalled native session.
//
//   node src/watchdog/scripted-stall.ts <issue-number>
//
// Environment: OSTROM_REPO, OSTROM_ALLOWLIST, OSTROM_DB (use a new file), OSTROM_WORKER_REPO and
// OSTROM_WORKER_BASE. The issue is only where a Router reply would go; nothing is posted unless
// `ostromd send` runs on this store, so never run `send` on it.

import { join } from "node:path";

import { loadConfig, loadWorkerConfig } from "../ostromd/config.ts";
import { sessionConfig } from "../session/config.ts";
import { codexRunner } from "../session/runner.ts";
import { createWorktree } from "../session/worktree.ts";
import { Store } from "../store/store.ts";

export const STALL_BRIEF = [
  "This is a scripted stall for Ostrom's Watchdog test.",
  "Ask the operator exactly one question: Which colour should the scripted stall use, red or blue?",
  "Wait for the answer. Do not make any edits or run commands.",
].join("\n");

async function scriptedStall(issue: number): Promise<void> {
  const config = loadConfig(process.env);
  const worker = loadWorkerConfig(process.env);
  const store = Store.open(config.dbPath);
  try {
    const source = store.ensureSource("github", config.repo);
    const url = `https://github.com/${config.repo}/issues/${issue}`;
    const threadId = store.ensureThread({ sourceId: source.id, kind: "issue", number: issue, url });
    const trigger = store.insertTrigger({
      sourceId: source.id,
      sourceEventId: `scripted-stall-${Date.now()}`,
      threadId,
      kind: "issue_comment",
      actor: config.allowlist[0]!,
      url: `${url}#scripted-stall`,
      occurredAt: new Date(),
    });
    const { ticketId } = store.recordDecision({
      threadId,
      triggerIds: [trigger.id],
      verdict: "accept",
      reason: "Scripted stall: the Worker asks a question and waits.",
    });
    const assignmentId = store.createAssignment(ticketId!);
    const path = join(worker.worktrees, `ticket-${ticketId}-${assignmentId}`);
    const branch = await createWorktree(worker.repo, path, worker.base, ticketId!, assignmentId);
    store.configureNativeAssignment(assignmentId, {
      harness: "codex", model: "gpt-6-luna", worktree: path, branch, machine: worker.machine,
    });
    const { codex } = sessionConfig(process.env);
    const session = codexRunner(codex).start({
      cwd: path,
      model: "gpt-6-luna",
      writableDirs: [join(worker.repo, ".git")],
      onSessionId: (sessionId) => store.startNativeAssignment(assignmentId, sessionId),
    });
    const result = await session.send(STALL_BRIEF);
    if (result.status !== "completed" || result.sessionId === null) throw new Error("scripted Worker did not start");
    process.stdout.write(`${JSON.stringify({ ticketId, assignmentId, sessionId: result.sessionId, branch, worktree: path })}\n`);
  } finally {
    store.close();
  }
}

if (import.meta.main) {
  const issue = process.argv[2] ?? "";
  if (process.argv.length !== 3 || !/^[1-9]\d*$/.test(issue)) {
    process.stderr.write("usage: node src/watchdog/scripted-stall.ts <issue-number>\n");
    process.exitCode = 2;
  } else {
    try {
      await scriptedStall(Number(issue));
    } catch (error) {
      process.stderr.write(`scripted-stall: ${error instanceof Error ? error.message : String(error)}\n`);
      process.exitCode = 1;
    }
  }
}
