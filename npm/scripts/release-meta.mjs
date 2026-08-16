import { appendFileSync } from 'node:fs';
import { cargoVersion, config } from './lib.mjs';

const output = process.env.GITHUB_OUTPUT;
if (!output) throw new Error('GITHUB_OUTPUT is required');

const version = cargoVersion();
const isRelease = process.env.GITHUB_EVENT_NAME === 'push';
if (isRelease && process.env.GITHUB_REF_NAME !== `v${version}`) {
  throw new Error(
    `release tag ${process.env.GITHUB_REF_NAME} does not match Cargo version v${version}`,
  );
}

const include = config.platforms.map((platform) => ({
  os: platform.runner,
  target: platform.target,
  platform: platform.platform,
  ext: platform.ext,
  binaries: config.binaryNames.map((name) => `${name}${platform.ext}`).join(','),
  artifactPaths: config.binaryNames
    .map(
      (name) =>
        `target/${platform.target}/release/${name}${platform.ext}`,
    )
    .join('\n'),
}));
const verificationPlatform = config.platforms.find(
  ({ nodeKey }) => nodeKey === 'linux-x64',
);
if (!verificationPlatform) {
  throw new Error(
    'publish config must include linux-x64 for package verification',
  );
}

appendFileSync(output, `version=${version}\n`);
appendFileSync(output, `is-release=${isRelease}\n`);
appendFileSync(output, `npm-tag=latest\n`);
appendFileSync(output, `matrix=${JSON.stringify({ include })}\n`);
appendFileSync(output, `verification-dir=${verificationPlatform.dir}\n`);
appendFileSync(output, `verification-binaries=${config.binaryNames.join(',')}\n`);
appendFileSync(output, `main-dir=${config.mainPackage.name}\n`);
