// ostromd: the daemon that will run intake, filtering, fan-out, the outbox and the Watchdog.
// See docs/direction.md. Nothing is implemented yet, so running it fails rather than pretending.

export function main(_argv: readonly string[]): number {
  process.stderr.write("ostromd: not implemented yet\n");
  return 1;
}

if (import.meta.main) {
  process.exitCode = main(process.argv.slice(2));
}
