// Assert a cargo test run executed the expected number of tests.
//
// Exit status alone is not enough: a suite that stops discovering its tests
// exits 0 having run nothing, which is exactly how an import silently stops
// being covered. This sums every `test result:` line and compares the total.
import { readFileSync } from 'node:fs';

const [logPath, expectedRaw] = process.argv.slice(2);
const expected = Number(expectedRaw);
const log = readFileSync(logPath, 'utf8');

let passed = 0;
let failed = 0;
for (const line of log.split('\n')) {
  const match = line.match(/^test result: \w+\. (\d+) passed; (\d+) failed;/);
  if (match) {
    passed += Number(match[1]);
    failed += Number(match[2]);
  }
}

if (failed !== 0) {
  console.log(`::error title=ethogram suite failed::${failed} failing of ${passed + failed}`);
  process.exit(1);
}
if (passed !== expected) {
  console.log(
    `::error title=ethogram test count moved::expected ${expected} executed, saw ${passed}. ` +
      'If this is a deliberate change, update the count here in the same commit.',
  );
  process.exit(1);
}
console.log(`ethogram suite: ${passed} executed, 0 failed`);
