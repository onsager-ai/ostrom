import { resolve } from 'node:path';
import { ROOT, argValue, config, packageDirs } from './lib.mjs';

const args = process.argv.slice(2);
const stagingRoot = resolve(
  ROOT,
  argValue(args, '--staging', config.stagingDir),
);

for (const pkg of packageDirs(stagingRoot)) {
  process.stdout.write(`${pkg.dir}\t${pkg.name}\n`);
}
