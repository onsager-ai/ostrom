// ostromd: the daemon that will run intake, filtering, fan-out, the outbox and the Watchdog.
// See docs/direction.md. For now it runs one pass of intake or of the outbox per invocation;
// scheduling the passes comes later.

import { execGh, type GhRunner } from "../github/gh.ts";
import { sendDueReplies } from "../github/outbox.ts";
import { pollOnce } from "../github/poller.ts";
import { Store } from "../store/store.ts";
import { loadConfig } from "./config.ts";

const USAGE = `usage: ostromd <command>

commands:
  poll   read new GitHub events once; print each candidate trigger as a JSON line
  send   post due outbox replies once

environment: OSTROM_REPO, OSTROM_ALLOWLIST, OSTROM_DB
`;

export interface MainDeps {
  env: Record<string, string | undefined>;
  gh: GhRunner;
  stdout: (line: string) => void;
  stderr: (line: string) => void;
}

const defaults: MainDeps = {
  env: process.env,
  gh: execGh,
  stdout: (line) => process.stdout.write(`${line}\n`),
  stderr: (line) => process.stderr.write(`${line}\n`),
};

/** Runs one command and returns its exit code. Anything that did not do its job is non-zero. */
export async function main(argv: readonly string[], deps: MainDeps = defaults): Promise<number> {
  const [command, ...rest] = argv;
  if ((command !== "poll" && command !== "send") || rest.length > 0) {
    deps.stderr(USAGE.trimEnd());
    return 2;
  }
  let store: Store | undefined;
  try {
    const config = loadConfig(deps.env);
    store = Store.open(config.dbPath);
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
