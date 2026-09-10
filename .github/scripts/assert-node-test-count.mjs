// Assert a `node --test` run executed the expected number of tests.
//
// Counts every reporter summary rather than grepping for a single global
// `fail 0`: with more than one package in the workspace, `pnpm -r test`
// prints one summary per package, and a grep for `fail 0` matches whenever
// *any* package reports it -- including while another reports failures.
import { readFileSync } from 'node:fs';

const [logPath, expectedRaw] = process.argv.slice(2);
const expected = Number(expectedRaw);
const log = readFileSync(logPath, 'utf8');

const sum = (label) =>
  [...log.matchAll(new RegExp(`^.{0,4}${label} (\\d+)$`, 'gm'))].reduce(
    (total, match) => total + Number(match[1]),
    0,
  );

const tests = sum('tests');
const failed = sum('fail');
const summaries = [...log.matchAll(/^.{0,4}tests \d+$/gm)].length;

if (summaries === 0) {
  console.log('::error title=no test summary::the reporter printed no `tests N` line');
  process.exit(1);
}
if (failed !== 0) {
  console.log(`::error title=TypeScript suite failed::${failed} failing of ${tests}`);
  process.exit(1);
}
if (tests !== expected) {
  console.log(
    `::error title=TypeScript test count moved::expected ${expected} executed, saw ${tests} ` +
      `across ${summaries} package summaries. Update the count here in the same commit.`,
  );
  process.exit(1);
}
console.log(`TypeScript: ${tests} executed across ${summaries} summaries, 0 failed`);
