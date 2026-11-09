// The operator CLI. Milestone 1 starts with a local, read-only outbox preview; the daemon retains
// its existing commands until the later CLI migration.

import { renderPreview } from "../github/preview.ts";
import { Store } from "../store/store.ts";

const USAGE = `usage: ostrom preview
       ostrom extend <ticket> [--time M] [--turns N] [--tokens N]

Renders the next outbox send pass as Markdown, or raises a ticket's stored budget limits.`;

export async function main(argv: readonly string[], env: Record<string, string | undefined> = process.env): Promise<number> {
  const path = env["OSTROM_DB"]?.trim() ?? "";
  if (path === "") {
    process.stderr.write("ostrom: OSTROM_DB must be the path to the store\n");
    return 1;
  }
  const store = Store.open(path);
  try {
    if (argv.length === 1 && argv[0] === "preview") {
      process.stdout.write(renderPreview(store));
      return 0;
    }
    const extension = parseExtend(argv);
    if (extension === null) {
      process.stderr.write(`${USAGE}\n`);
      return 2;
    }
    store.extendTicketBudget(extension.ticketId, extension.increases);
    const ticket = store.getTicket(extension.ticketId);
    const budget = store.ticketBudget(extension.ticketId);
    process.stdout.write(`${JSON.stringify({ ticketId: ticket.id, state: ticket.state, budget })}\n`);
    return 0;
  } catch (error) {
    process.stderr.write(`ostrom: ${error instanceof Error ? error.message : String(error)}\n`);
    return 1;
  } finally {
    store.close();
  }
}

function parseExtend(argv: readonly string[]): { ticketId: number; increases: { timeMinutes?: number; turns?: number; tokens?: number } } | null {
  if (argv[0] !== "extend" || !/^[1-9]\d*$/.test(argv[1] ?? "")) return null;
  const increases: { timeMinutes?: number; turns?: number; tokens?: number } = {};
  for (let index = 2; index < argv.length; index += 2) {
    const flag = argv[index];
    const value = argv[index + 1] ?? "";
    if (!/^[1-9]\d*$/.test(value)) return null;
    if (flag === "--time") increases.timeMinutes = Number(value);
    else if (flag === "--turns") increases.turns = Number(value);
    else if (flag === "--tokens") increases.tokens = Number(value);
    else return null;
  }
  return Object.keys(increases).length === 0 ? null : { ticketId: Number(argv[1]), increases };
}

if (import.meta.main) process.exitCode = await main(process.argv.slice(2));
