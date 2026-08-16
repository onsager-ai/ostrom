import { mkdirSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { ROOT, argValue, binaryNames, config } from './lib.mjs';

const outputRoot = resolve(
  ROOT,
  argValue(process.argv.slice(2), '--output', 'target/test-artifacts'),
);
const signatures = {
  linux: [0x7f, 0x45, 0x4c, 0x46],
  darwin: [0xcf, 0xfa, 0xed, 0xfe],
  win32: [0x4d, 0x5a],
};

for (const platform of config.platforms) {
  const artifactDir = join(outputRoot, `binary-${platform.platform}`);
  mkdirSync(artifactDir, { recursive: true });
  for (const binary of binaryNames(platform)) {
    writeFileSync(
      join(artifactDir, binary),
      Buffer.from([...signatures[platform.os], 0x00, 0x01, 0x02, 0x03]),
    );
  }
}
