// ostromd: the daemon that will run intake, filtering, fan-out, the outbox and the Watchdog.
// See docs/direction.md. For now it runs one pass of intake, the outbox, Worker dispatch or Worker
// observation per invocation; scheduling the passes comes later.

import { execGh, type GhRunner } from "../github/gh.ts";
import { sendDueReplies } from "../github/outbox.ts";
import { pollOnce } from "../github/poller.ts";
import { Store } from "../store/store.ts";
import { processRunner, T3Cli, type T3Runner } from "../t3/runner.ts";
import { dispatch, observe } from "../worker/dispatch.ts";
import { loadConfig, loadWorkerConfig } from "./config.ts";

const USAGE = `usage: ostromd <command>

commands:
  poll   read new GitHub events once; print each candidate trigger as a JSON line
  send   post due outbox replies once
  dispatch <ticket-id>
         start a T3 Code Worker for an open ticket; print the assignment as a JSON line
  observe
         read every running Worker's T3 thread once; subscribe tickets to their PRs and
         print each observation as a JSON line

environment: OSTROM_REPO, OSTROM_ALLOWLIST, OSTROM_DB;
  dispatch also needs OSTROM_T3_PROJECT, OSTROM_WORKER_BASE, OSTROM_WORKER_PROVIDER,
  OSTROM_WORKER_MODEL
`;

export interface MainDeps {
  env: Record<string, string | undefined>;
  gh: GhRunner;
  t3: T3Runner;
  stdout: (line: string) => void;
  stderr: (line: string) => void;
}

const defaults: MainDeps = {
  env: process.env,
  gh: execGh,
  t3: processRunner(),
  stdout: (line) => process.stdout.write(`${line}\n`),
  stderr: (line) => process.stderr.write(`${line}\n`),
};

/** Runs one command and returns its exit code. Anything that did not do its job is non-zero. */
export async function main(argv: readonly string[], deps: MainDeps = defaults): Promise<number> {
  const [command, ...rest] = argv;
  const ticketArg = rest[0];
  const valid =
    command === "dispatch"
      ? rest.length === 1 && /^[1-9]\d*$/.test(ticketArg!)
      : (command === "poll" || command === "send" || command === "observe") && rest.length === 0;
  if (!valid) {
    deps.stderr(USAGE.trimEnd());
    return 2;
  }
  let store: Store | undefined;
  try {
    const config = loadConfig(deps.env);
    store = Store.open(config.dbPath);
    if (command === "dispatch") {
      const worker = loadWorkerConfig(deps.env);
      const ticketId = Number(ticketArg);
      const dispatched = await dispatch(store, new T3Cli(deps.t3), worker, ticketId);
      deps.stdout(JSON.stringify({ ticketId, ...dispatched }));
      return 0;
    }
    if (command === "observe") {
      const pass = await observe(store, new T3Cli(deps.t3), config.repo);
      for (const observed of pass.observed) deps.stdout(JSON.stringify(observed));
      for (const failure of pass.failed) {
        deps.stderr(`ostromd observe: assignment ${failure.assignmentId} failed: ${failure.error}`);
      }
      deps.stderr(`ostromd observe: ${pass.observed.length} observed, ${pass.failed.length} failed`);
      return pass.failed.length === 0 ? 0 : 1;
    }
    if (command === "poll") {
      const result = await pollOnce({ store, gh: deps.gh, repo: config.repo, allowlist: config.allowlist });
      for (const candidate of result.candidates) deps.stdout(JSON.stringify(candidate));
      deps.stderr(
        `ostromd poll: ${result.candidates.length} new, ${result.duplicates} already stored, ` +
          `${result.droppedOwn} own replies dropped, ${result.droppedNotAllowed} not on the allowlist`,
      );
      return 0;
    }
    const result = await sendDueReplies({ store, gh: deps.gh, repo: config.repo });
    for (const failure of result.failed) deps.stderr(`ostromd send: ${failure.id} failed: ${failure.error}`);
    deps.stderr(
      `ostromd send: ${result.sent.length} sent, ${result.found.length} already posted, ` +
        `${result.failed.length} failed`,
    );
    return result.failed.length === 0 ? 0 : 1;
  } catch (error) {
    deps.stderr(`ostromd ${command}: ${error instanceof Error ? error.message : String(error)}`);
    return 1;
  } finally {
    store?.close();
  }
}

if (import.meta.main) {
  process.exitCode = await main(process.argv.slice(2));
}
