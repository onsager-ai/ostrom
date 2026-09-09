import {
  PLATFORM_WAIT_ATTEMPTS,
  PLATFORM_WAIT_DELAY_MS,
  argValue,
  cargoVersion,
  config,
  platformPackageName,
  registryHasVersion,
} from './lib.mjs';

const args = process.argv.slice(2);
const version = argValue(args, '--version', cargoVersion());
const attempts = Number(
  argValue(args, '--attempts', String(PLATFORM_WAIT_ATTEMPTS)),
);
const delayMs = Number(
  argValue(args, '--delay-ms', String(PLATFORM_WAIT_DELAY_MS)),
);

function visible(packageName) {
  return registryHasVersion(packageName, version);
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
