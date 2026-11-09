// ostromd: the daemon that runs intake, filtering, fan-out, the outbox and the Watchdog.
// See docs/direction.md. Each pass is its own command; `tick` runs them all once in order, and
// `run` repeats `tick`. Installing it as a service is not done here.

import { execGh, type GhRunner } from "../github/gh.ts";
import { sendDueReplies } from "../github/outbox.ts";
import { pollOnce } from "../github/poller.ts";
import { routeCandidates } from "../router/router.ts";
import { Store } from "../store/store.ts";
import { processRunner, T3Cli, type T3Runner } from "../t3/runner.ts";
import { watchdog } from "../watchdog/watchdog.ts";
import { dispatch, observe } from "../worker/dispatch.ts";
import { routeFollowUps } from "../worker/follow-up.ts";
import { loadConfig, loadRouterConfig, loadWatchdogConfig, loadWorkerConfig } from "./config.ts";

/**
 * The passes one tick runs, in this order:
 * - poll: new GitHub events become triggers; everything after works on what is stored.
 * - observe: a Worker's newly opened PR is subscribed to its ticket before anything routes, so a
 *   review comment polled in the same tick is a follow-up, not a candidate for the Router.
 * - route: follow-ups, including those, reach their Workers.
 * - watchdog: after a follow-up may have unblocked a Worker, and before the Router, so a new
 *   stall is flagged in this tick's batch.
 * - route-candidates: the Router judges candidates, outcomes and stalls and queues replies.
 * - send: replies queued by this tick's Router pass go out in this tick.
 * Each pass reads and writes only the store and its own neighbour, so none depends on an earlier
 * one succeeding: a failed pass is reported and the rest still run.
 */
export const TICK = ["poll", "observe", "route", "watchdog", "route-candidates", "send"] as const;

const USAGE = `usage: ostromd <command>

commands:
  poll   read new GitHub events once; print each candidate trigger as a JSON line
  send   post due outbox replies once
  dispatch <ticket-id>
         start a T3 Code Worker for an open ticket; print the assignment as a JSON line
  observe
         read every running Worker's T3 thread once; subscribe tickets to their PRs and
         print each observation as a JSON line
  route  send each new trigger on a subscribed thread to its ticket's Worker once; print each
         delivery as a JSON line
  watchdog
         check every running Worker once with Semon's stalls read and its T3 thread state;
         record each new stall episode for the Router's next batch and print each check as a
         JSON line
  route-candidates
         one Router pass: ask the Router to judge undecided triggers, word outcome replies and
         answer flagged stalls (wait or stop), record its decisions, dispatch accepted tickets,
         queue replies and end finished or stopped tickets; print each record as a JSON line
  tick   run every pass once: ${TICK.join(" → ")}; a failed pass is reported, the rest still
         run, and the tick exits non-zero
  run --interval <seconds>
         repeat tick, waiting that long after each, until SIGINT or SIGTERM

environment: OSTROM_REPO, OSTROM_ALLOWLIST, OSTROM_DB;
  dispatch also needs OSTROM_T3_PROJECT, OSTROM_WORKER_BASE, OSTROM_WORKER_PROVIDER,
  OSTROM_WORKER_MODEL; route-candidates needs those and OSTROM_ROUTER_PROVIDER,
  OSTROM_ROUTER_MODEL; watchdog needs OSTROM_STALL_MINUTES; tick and run need all of them
`;

export interface MainDeps {
  env: Record<string, string | undefined>;
  gh: GhRunner;
  t3: T3Runner;
  /** Runs `semon`; same shape as the t3 runner. */
  semon: T3Runner;
  stdout: (line: string) => void;
  stderr: (line: string) => void;
  /** Waits between ticks of `run`; resolves early when `stop` aborts. */
  sleep: (ms: number, stop: AbortSignal) => Promise<void>;
  /** Aborted when `run` should stop after the current tick. */
  stop: AbortSignal;
}

function sleep(ms: number, stop: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const timer = setTimeout(done, ms);
    stop.addEventListener("abort", done, { once: true });
    function done() {
      clearTimeout(timer);
      stop.removeEventListener("abort", done);
      resolve();
    }
  });
}

const defaults: MainDeps = {
  env: process.env,
  gh: execGh,
  t3: processRunner(),
  semon: processRunner("semon"),
  stdout: (line) => process.stdout.write(`${line}\n`),
  stderr: (line) => process.stderr.write(`${line}\n`),
  sleep,
  stop: new AbortController().signal,
};

/** Checks every variable a tick needs, so a misconfigured tick or run fails before any pass. */
function loadTickConfig(env: MainDeps["env"]): void {
  loadConfig(env);
  loadWorkerConfig(env);
  loadRouterConfig(env);
  loadWatchdogConfig(env);
}

/** One tick: every pass in TICK order, each opening the store itself and reporting as its command does. */
async function tick(deps: MainDeps): Promise<number> {
  try {
    loadTickConfig(deps.env);
  } catch (error) {
    deps.stderr(`ostromd tick: ${error instanceof Error ? error.message : String(error)}`);
    return 1;
  }
  const failed: string[] = [];
  for (const step of TICK) {
    const code = await main([step], deps);
    if (code !== 0) failed.push(`${step} (exit ${code})`);
  }
  deps.stderr(
    failed.length === 0 ? `ostromd tick: all ${TICK.length} passes ran` : `ostromd tick: failed: ${failed.join(", ")}`,
  );
  return failed.length === 0 ? 0 : 1;
}

/** Repeats `tick` until `stop` aborts. Exits with the last tick's code. */
async function run(intervalSeconds: number, deps: MainDeps): Promise<number> {
  try {
    loadTickConfig(deps.env);
  } catch (error) {
    deps.stderr(`ostromd run: ${error instanceof Error ? error.message : String(error)}`);
    return 1;
  }
  let code = 0;
  while (!deps.stop.aborted) {
    code = await tick(deps);
    if (deps.stop.aborted) break;
    await deps.sleep(intervalSeconds * 1000, deps.stop);
  }
  return code;
}

/** Runs one command and returns its exit code. Anything that did not do its job is non-zero. */
export async function main(argv: readonly string[], deps: MainDeps = defaults): Promise<number> {
  const [command, ...rest] = argv;
  const ticketArg = rest[0];
  const valid =
    command === "dispatch"
      ? rest.length === 1 && /^[1-9]\d*$/.test(ticketArg!)
      : command === "run"
        ? rest.length === 2 && rest[0] === "--interval" && /^[1-9]\d*$/.test(rest[1]!)
        : (command === "poll" ||
            command === "send" ||
            command === "observe" ||
            command === "route" ||
            command === "watchdog" ||
            command === "route-candidates" ||
            command === "tick") &&
          rest.length === 0;
  if (!valid) {
    deps.stderr(USAGE.trimEnd());
    return 2;
  }
  if (command === "tick") return tick(deps);
  if (command === "run") return run(Number(rest[1]), deps);
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
    if (command === "route") {
      const pass = await routeFollowUps(store, new T3Cli(deps.t3));
      for (const delivered of pass.delivered) deps.stdout(JSON.stringify(delivered));
      for (const failure of pass.failed) {
        deps.stderr(
          `ostromd route: trigger ${failure.triggerId} to ticket ${failure.ticketId} failed: ${failure.error}`,
        );
      }
      deps.stderr(`ostromd route: ${pass.delivered.length} delivered, ${pass.failed.length} failed`);
      return pass.failed.length === 0 ? 0 : 1;
    }
    if (command === "watchdog") {
      const { idleMinutes } = loadWatchdogConfig(deps.env);
      const pass = await watchdog({
        store,
        t3: new T3Cli(deps.t3),
        semon: deps.semon,
        idleMinutes,
        now: () => new Date(),
      });
      for (const checked of pass.checked) deps.stdout(JSON.stringify(checked));
      for (const failure of pass.failed) deps.stderr(`ostromd watchdog: ${failure.what} failed: ${failure.error}`);
      const flagged = pass.checked.filter((checked) => checked.newEpisode).length;
      const stalled = pass.checked.filter((checked) => checked.stallId !== null).length;
      deps.stderr(
        `ostromd watchdog: ${pass.checked.length} checked, ${stalled} stalled, ${flagged} newly flagged, ` +
          `${pass.failed.length} failed`,
      );
      return pass.failed.length === 0 ? 0 : 1;
    }
    if (command === "route-candidates") {
      const worker = loadWorkerConfig(deps.env);
      const router = loadRouterConfig(deps.env);
      const pass = await routeCandidates({ store, gh: deps.gh, t3: new T3Cli(deps.t3), repo: config.repo, worker, router });
      if (pass.router !== null) deps.stdout(JSON.stringify({ type: "router", ...pass.router }));
      for (const decision of pass.decisions) deps.stdout(JSON.stringify({ type: "decision", ...decision }));
      for (const outcome of pass.outcomes) deps.stdout(JSON.stringify({ type: "outcome", ...outcome }));
      for (const stall of pass.stalls) deps.stdout(JSON.stringify({ type: "stall", ...stall }));
      for (const dispatched of pass.dispatched) deps.stdout(JSON.stringify({ type: "dispatch", ...dispatched }));
      for (const failure of pass.failed) deps.stderr(`ostromd route-candidates: ${failure.what} failed: ${failure.error}`);
      deps.stderr(
        `ostromd route-candidates: ${pass.decisions.length} decisions, ${pass.outcomes.length} outcome replies, ` +
          `${pass.stalls.length} stalls answered, ${pass.dispatched.length} dispatched, ${pass.failed.length} failed`,
      );
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
  const stop = new AbortController();
  // Only `run` finishes its current tick on a signal; a second signal, or any other command, stops as usual.
  if (process.argv[2] === "run") {
    for (const signal of ["SIGINT", "SIGTERM"] as const) process.once(signal, () => stop.abort());
  }
  process.exitCode = await main(process.argv.slice(2), { ...defaults, stop: stop.signal });
}
