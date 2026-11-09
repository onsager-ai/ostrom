// The scripted stall (docs/direction.md, milestone 1 pass criterion): seeds a throwaway store with
// one accepted ticket and starts a real T3 Worker for it whose brief makes it ask a question and
// wait. After OSTROM_STALL_MINUTES the Watchdog flags it.
//
//   node src/watchdog/scripted-stall.ts <issue-number>
//
// Environment: OSTROM_REPO, OSTROM_ALLOWLIST, OSTROM_DB (use a new file), and for the T3 thread
// this script still starts (native Workers run headless and cannot ask a question):
// OSTROM_T3_PROJECT, OSTROM_WORKER_BASE, OSTROM_STALL_PROVIDER, OSTROM_STALL_MODEL.
// The issue is only where a Router reply would go; nothing is posted unless `ostromd send` runs
// on this store, so never run `send` on it. The procedure, and how to clean up the worktree and
// branch the Worker gets, is in docs/watchdog.md.

import { loadConfig } from "../ostromd/config.ts";
import { Store } from "../store/store.ts";
import { processRunner, T3Cli } from "../t3/runner.ts";

export const STALL_BRIEF = [
  "This is a scripted stall for Ostrom's Watchdog test. Do not read GitHub, run commands or edit files.",
  "Ask the operator exactly one question with your AskUserQuestion tool: \"Which colour should the",
  'scripted stall use?", with the options "red" and "blue". Then wait for the answer and do nothing else.',
].join("\n");

async function scriptedStall(issue: number): Promise<void> {
  const config = loadConfig(process.env);
  const read = (name: string) => {
    const value = process.env[name]?.trim() ?? "";
    if (value === "") throw new Error(`${name} must be set`);
    return value;
  };
  const t3 = { project: read("OSTROM_T3_PROJECT"), base: read("OSTROM_WORKER_BASE"), provider: read("OSTROM_STALL_PROVIDER"), model: read("OSTROM_STALL_MODEL") };
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
    const branch = `ostrom/ticket-${ticketId}-${assignmentId}`;
    const { threadId: t3ThreadId } = await new T3Cli(processRunner()).startThread({
      project: t3.project,
      title: `Ostrom scripted stall: ticket ${ticketId}`,
      brief: STALL_BRIEF,
      worktreeBase: t3.base,
      branch,
      provider: t3.provider,
      model: t3.model,
    });
    store.startAssignment(assignmentId, t3ThreadId);
    process.stdout.write(`${JSON.stringify({ ticketId, assignmentId, t3ThreadId, branch })}\n`);
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
