import { execFileSync } from 'node:child_process';
import {
  argValue,
  cargoVersion,
  config,
  platformPackageName,
} from './lib.mjs';

const args = process.argv.slice(2);
const version = argValue(args, '--version', cargoVersion());
const attempts = Number(argValue(args, '--attempts', '20'));
const delayMs = Number(argValue(args, '--delay-ms', '15000'));

function visible(packageName) {
  try {
    const output = execFileSync(
      'npm',
      ['view', `${packageName}@${version}`, 'version'],
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] },
    ).trim();
    return output === version;
  } catch {
    return false;
  }
}

const sleep = (milliseconds) =>
  new Promise((resolve) => setTimeout(resolve, milliseconds));

for (let attempt = 1; attempt <= attempts; attempt += 1) {
  const missing = config.platforms
    .map(platformPackageName)
    .filter((packageName) => !visible(packageName));
  if (missing.length === 0) {
    console.log(`all platform packages are visible at ${version}`);
    process.exit(0);
  }
  if (attempt === attempts) {
    throw new Error(
      `platform packages did not propagate: ${missing.join(', ')}`,
    );
  }
  console.log(
    `registry propagation attempt ${attempt}/${attempts}; waiting for ${missing.join(', ')}`,
  );
  await sleep(delayMs);
}
