import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { ROOT, argValue, cargoVersion } from './lib.mjs';

export function assertVersion(output, expected) {
  const actual = output
    .trim()
    .match(/(?:^|\s)(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)$/)?.[1];
  if (actual === expected) return;
  throw new Error(
    `version mismatch: binary reports ${actual ?? JSON.stringify(output)}, ` +
      `package expects ${expected}`,
  );
}

if (
  process.argv[1] &&
  fileURLToPath(import.meta.url) === resolve(process.argv[1])
) {
  const args = process.argv.slice(2);
  const packagePath = argValue(args, '--package', undefined);
  const expected = packagePath
    ? JSON.parse(readFileSync(resolve(ROOT, packagePath), 'utf8')).version
    : argValue(args, '--expected', cargoVersion());
  const versionOutput = resolve(ROOT, argValue(args, '--version-output'));
  try {
    assertVersion(readFileSync(versionOutput, 'utf8'), expected);
    console.log(`binary and package version match: ${expected}`);
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
