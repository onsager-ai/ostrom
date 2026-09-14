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
  // release.yml checks out `refs/tags/<tag>`, but what matters is the commit
  // actually on disk. The tag must exist, and HEAD must be the commit it
  // points to. Otherwise a same-named branch, or a checkout that resolved to
  // something else, would publish absent packages from the wrong commit, with
  // a gitHead that matches nothing on the registry. Both are refused here,
  // before any package is staged.
  const git = (revision) =>
    execFileSync('git', ['rev-parse', '--verify', '--quiet', revision], {
      cwd: ROOT,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
    }).trim();
  let tagCommit;
  try {
    tagCommit = git(`refs/tags/${dispatchTag}^{commit}`);
  } catch {
    throw new Error(
      `dispatch tag ${dispatchTag} does not resolve to an existing tag in this checkout`,
    );
  }
  const headCommit = git('HEAD');
  if (tagCommit !== headCommit) {
    throw new Error(
      `dispatch tag ${dispatchTag} is ${tagCommit}, but this checkout is ${headCommit}; ` +
        'refusing to publish from any commit other than the tag\'s',
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
