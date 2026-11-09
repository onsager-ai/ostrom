// The operator CLI. Milestone 1 starts with a local, read-only outbox preview; the daemon retains
// its existing commands until the later CLI migration.

import { renderPreview } from "../github/preview.ts";
import { Store } from "../store/store.ts";

const USAGE = "usage: ostrom preview\n\nRenders the next outbox send pass as Markdown without sending anything.";

export async function main(argv: readonly string[], env: Record<string, string | undefined> = process.env): Promise<number> {
  if (argv.length !== 1 || argv[0] !== "preview") {
    process.stderr.write(`${USAGE}\n`);
    return 2;
  }
  const path = env["OSTROM_DB"]?.trim() ?? "";
  if (path === "") {
    process.stderr.write("ostrom preview: OSTROM_DB must be the path to the store\n");
    return 1;
  }
  const store = Store.open(path);
  try {
    process.stdout.write(renderPreview(store));
    return 0;
  } finally {
    store.close();
  }
}

if (import.meta.main) process.exitCode = await main(process.argv.slice(2));
