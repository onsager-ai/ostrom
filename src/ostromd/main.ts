// ostromd: the daemon that runs intake, filtering, fan-out, the outbox and the Watchdog.
// See docs/direction.md. The operator starts only `run`; individual passes remain under `dev`.

import { defaultSocketPath, localApi } from "../api/server.ts";
import { operatorControls, routeCommands } from "../commands/commands.ts";
import { renderPreview } from "../github/preview.ts";
import { execGh, type GhRunner } from "../github/gh.ts";
import { sendDueReplies } from "../github/outbox.ts";
import { processRunner, type ProcessRunner } from "../process.ts";
import { pollOnce } from "../github/poller.ts";
import { routeCandidates } from "../router/router.ts";
import { sessionConfig } from "../session/config.ts";
import { codexSessionsDir } from "../session/rollout.ts";
import { claudeRunner, codexRunner, type Runner } from "../session/runner.ts";
import { Store } from "../store/store.ts";
import { advanceStuckLadder } from "../watchdog/stuck.ts";
import { watchdog } from "../watchdog/watchdog.ts";
import { observe } from "../worker/dispatch.ts";
import { routeFollowUps } from "../worker/follow-up.ts";
import { Workers, type WorkersDeps } from "../worker/workers.ts";
import { loadBudgetDefaults, loadConfig, loadRouterConfig, loadWatchdogConfig, loadWorkerConfig } from "./config.ts";

/**
 * The passes one tick runs, in this order:
 * - poll: new GitHub events become triggers; everything after works on what is stored.
 * - observe: a Worker's newly opened PR is subscribed to its ticket before anything routes, so a
 *   review comment polled in the same tick is a follow-up, not a candidate for the Router.
 * - commands: fixed control words are authorized and consumed before any Worker delivery.
 * - route: ordinary follow-ups, including those, reach their Workers.
 * - watchdog: after a follow-up may have unblocked a Worker, and before the Router, so a new
 *   stall is flagged in this tick's batch.
 * - route-candidates: the Router judges candidates, outcomes and stalls and queues replies.
 * - send: replies queued by this tick's Router pass go out in this tick.
 * Each pass reads and writes only the store and its own neighbour, so none depends on an earlier
 * one succeeding: a failed pass is reported and the rest still run.
 */
export const TICK = ["poll", "observe", "commands", "route", "watchdog", "route-candidates", "send"] as const;

const USAGE = `usage: ostromd run --interval <seconds>
       ostromd dev <pass> [ticket-id]

commands:
  run --interval <seconds>
         repeat the daemon loop until SIGINT or SIGTERM
  dev <pass>
         developer-only one-shot pass: poll, send, dispatch <ticket-id>, observe, commands,
         route, watchdog, route-candidates or tick

run needs OSTROM_REPO, OSTROM_ALLOWLIST, OSTROM_DB and the worker/router/watchdog configuration;
see docs/running.md. Developer passes retain their existing environment requirements.
`;

export interface MainDeps {
  env: Record<string, string | undefined>;
  gh: GhRunner;
  semon: ProcessRunner;
  stdout: (line: string) => void;
  stderr: (line: string) => void;
  /** Waits between ticks of `run`; resolves early when `stop` aborts. */
  sleep: (ms: number, stop: AbortSignal) => Promise<void>;
  /** Aborted when `run` should stop after the current tick. */
  stop: AbortSignal;
  /** Session runners; default to the real Claude and Codex SDKs configured from `env`. */
  runners?: Record<"claude" | "codex", Runner>;
  /** Where Codex rollout records live; default ~/.codex/sessions. */
  codexSessions?: string;
  addWorktree?: WorkersDeps["addWorktree"];
  /** The Workers a `run` shares across its ticks; one-shot commands make their own. */
  workers?: Workers;
}

function sleep(ms: number, stop: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    // Not unref'd: this timer is what keeps an idle `ostromd run` alive between ticks.
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
  loadBudgetDefaults(env);
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
  const owned = deps.workers === undefined ? openWorkers(deps) : undefined;
  if (owned !== undefined) logRecovery(owned.workers.recover(), deps);
  const shared = { ...deps, workers: deps.workers ?? owned!.workers };
  try {
    for (const step of TICK) {
      const code = await main([step], shared);
      if (code !== 0) failed.push(`${step} (exit ${code})`);
    }
  } finally {
    if (owned !== undefined) await owned.close();
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
  const owned = openWorkers(deps);
  const controls = operatorControls(owned.store, owned.workers, deps.gh, loadConfig(deps.env).repo);
  const api = localApi(owned.store, {
    socketPath: defaultSocketPath(deps.env),
    control: {
      ...controls,
      extend: async (ticketId, increases) => { owned.store.extendTicketBudget(ticketId, increases); },
      preview: async () => renderPreview(owned.store),
    },
  });
  logRecovery(owned.workers.recover(), deps);
  try {
    await api.start();
    while (!deps.stop.aborted) {
      code = await tick({ ...deps, workers: owned.workers });
      if (deps.stop.aborted) break;
      await deps.sleep(intervalSeconds * 1000, deps.stop);
    }
  } finally {
    await api.close();
    await owned.close();
  }
  return code;
}

function makeWorkers(deps: MainDeps, store: Store): Workers {
  return new Workers({
    store,
    config: loadWorkerConfig(deps.env),
    runners: deps.runners ?? realRunners(deps.env),
    codexSessions: deps.codexSessions ?? codexSessionsDir(),
    ...(deps.addWorktree === undefined ? {} : { addWorktree: deps.addWorktree }),
    log: (line) => deps.stderr(`ostromd: ${line}`),
  });
}

/** A Workers on its own store connection, so its turns outlive the store a single pass opens. */
function openWorkers(deps: MainDeps) {
  const store = Store.open(loadConfig(deps.env).dbPath);
  const workers = makeWorkers(deps, store);
  return {
    store,
    workers,
    close: async () => {
      try {
        await workers.drain();
      } finally {
        store.close();
      }
    },
  };
}

function realRunners(env: MainDeps["env"]): Record<"claude" | "codex", Runner> {
  const { claudeEnv, codex } = sessionConfig(env);
  return { claude: lazy(() => claudeRunner(claudeEnv)), codex: lazy(() => codexRunner(codex)) };
}

/** Builds an SDK runner on first use, so a harness that is never started is never constructed. */
function lazy(make: () => Runner): Runner {
  let runner: Runner | undefined;
  return {
    start: (options) => (runner ??= make()).start(options),
    resume: (options) => (runner ??= make()).resume(options),
  };
}

function logRecovery(pass: ReturnType<Workers["recover"]>, deps: MainDeps): void {
  if (pass.resumed.length + pass.abandoned.length === 0) return;
  deps.stderr(
    `ostromd recover: ${pass.resumed.length} resumed, ${pass.interrupted.length} interrupted, ` +
      `${pass.abandoned.length} never started`,
  );
}

/** Runs one command and returns its exit code. Anything that did not do its job is non-zero. */
export async function main(argv: readonly string[], deps: MainDeps = defaults): Promise<number> {
  const [requested, ...argvRest] = argv;
  const developer = requested === "dev";
  const [command, ...rest] = developer ? argvRest : [requested, ...argvRest];
  const ticketArg = rest[0];
  const valid =
    command === "dispatch"
      ? developer && rest.length === 1 && /^[1-9]\d*$/.test(ticketArg!)
      : command === "run"
        ? !developer && rest.length === 2 && rest[0] === "--interval" && /^[1-9]\d*$/.test(rest[1]!)
        : developer && (command === "poll" ||
            command === "send" ||
            command === "observe" ||
            command === "commands" ||
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
  let own: Workers | undefined;
  try {
    const config = loadConfig(deps.env);
    store = Store.open(config.dbPath);
    const openStore = store;
    // A one-shot command's Workers live until its turns finish; a run's are shared across ticks.
    const workers = () => deps.workers ?? (own ??= makeWorkers(deps, openStore));
    if (command === "dispatch") {
      const ticketId = Number(ticketArg);
      const dispatched = await workers().dispatch(ticketId);
      deps.stdout(JSON.stringify({ ticketId, ...dispatched }));
      return 0;
    }
    if (command === "observe") {
      const pass = await observe(store, deps.gh, config.repo);
      for (const observed of pass.observed) deps.stdout(JSON.stringify(observed));
      for (const failure of pass.failed) {
        deps.stderr(`ostromd observe: assignment ${failure.assignmentId} failed: ${failure.error}`);
      }
      deps.stderr(`ostromd observe: ${pass.observed.length} observed, ${pass.failed.length} failed`);
      return pass.failed.length === 0 ? 0 : 1;
    }
    if (command === "commands") {
      const pass = await routeCommands(store, workers(), deps.gh, config.repo);
      for (const handled of pass.handled) deps.stdout(JSON.stringify(handled));
      for (const failure of pass.failed) {
        deps.stderr(`ostromd commands: trigger ${failure.triggerId} for ticket ${failure.ticketId} failed: ${failure.error}`);
      }
      deps.stderr(`ostromd commands: ${pass.handled.length} handled, ${pass.failed.length} failed`);
      return pass.failed.length === 0 ? 0 : 1;
    }
    if (command === "route") {
      const pass = routeFollowUps(store, workers());
      for (const queued of pass.queued) deps.stdout(JSON.stringify(queued));
      for (const failure of pass.failed) {
        deps.stderr(
          `ostromd route: trigger ${failure.triggerId} to ticket ${failure.ticketId} failed: ${failure.error}`,
        );
      }
      deps.stderr(`ostromd route: ${pass.queued.length} queued, ${pass.failed.length} failed`);
      return pass.failed.length === 0 ? 0 : 1;
    }
    if (command === "watchdog") {
      const { idleMinutes } = loadWatchdogConfig(deps.env);
      const pass = await watchdog({ store, semon: deps.semon, idleMinutes });
      const ladder = await advanceStuckLadder(store, workers());
      for (const checked of pass.checked) deps.stdout(JSON.stringify(checked));
      for (const failure of pass.failed) deps.stderr(`ostromd watchdog: ${failure.what} failed: ${failure.error}`);
      const flagged = pass.checked.filter((checked) => checked.newEpisode).length;
      const stalled = pass.checked.filter((checked) => checked.stallId !== null).length;
      deps.stderr(
        `ostromd watchdog: ${pass.checked.length} checked, ${stalled} stalled, ${flagged} newly flagged, ` +
          `${ladder.recovered.length} recovering, ${ladder.published.length} public, ${pass.failed.length} failed`,
      );
      return pass.failed.length === 0 ? 0 : 1;
    }
    if (command === "route-candidates") {
      const router = loadRouterConfig(deps.env);
      const pass = await routeCandidates({
        store,
        gh: deps.gh,
        repo: config.repo,
        workers: workers(),
        router,
        runners: deps.runners ?? realRunners(deps.env),
        budget: loadBudgetDefaults(deps.env),
      });
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
    try {
      await own?.drain();
    } finally {
      store?.close();
    }
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
