import { execFileSync } from 'node:child_process';
import { appendFileSync } from 'node:fs';
import { ROOT, cargoVersion, config } from './lib.mjs';

const output = process.env.GITHUB_OUTPUT;
if (!output) throw new Error('GITHUB_OUTPUT is required');

const version = cargoVersion();
const eventName = process.env.GITHUB_EVENT_NAME;
const dispatchTag = (process.env.RELEASE_DISPATCH_TAG ?? '').trim();

// Two ways this run can be "a release": a tag push (today's behaviour), or a
// workflow_dispatch that names a `tag` input — the recovery path for a
// release whose build artifacts have expired (#585 §5). A plain dispatch
// with no tag stays a build-and-pack-only run, exactly as before.
const isTagPush = eventName === 'push';
const isDispatchRebuild = eventName === 'workflow_dispatch' && dispatchTag !== '';
const isRelease = isTagPush || isDispatchRebuild;

if (isTagPush && process.env.GITHUB_REF_NAME !== `v${version}`) {
  throw new Error(
    `release tag ${process.env.GITHUB_REF_NAME} does not match Cargo version v${version}`,
  );
}

if (isDispatchRebuild) {
  if (dispatchTag !== `v${version}`) {
    throw new Error(
      `dispatch tag ${dispatchTag} does not match Cargo version v${version} at this checkout`,
    );
  }
  // The checkout step already resolved `ref: <tag>`, which happily accepts a
  // branch or any other ref sharing the tag's name. Confirm an actual tag
  // exists before treating this as a recognised recovery build, so a typo or
  // a same-named branch fails loudly here instead of silently rebuilding the
  // wrong commit under a release label.
  try {
    execFileSync(
      'git',
      ['rev-parse', '--verify', '--quiet', `refs/tags/${dispatchTag}`],
      { cwd: ROOT, stdio: ['ignore', 'ignore', 'ignore'] },
    );
  } catch {
    throw new Error(
      `dispatch tag ${dispatchTag} does not resolve to an existing tag in this checkout`,
    );
  }
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
appendFileSync(output, `rebuild-of-tag=${isDispatchRebuild}\n`);
appendFileSync(output, `npm-tag=latest\n`);
appendFileSync(output, `matrix=${JSON.stringify({ include })}\n`);
appendFileSync(output, `verification-dir=${verificationPlatform.dir}\n`);
appendFileSync(output, `verification-binaries=${config.binaryNames.join(',')}\n`);
appendFileSync(output, `main-dir=${config.mainPackage.name}\n`);
