import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { dirname, join, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

export const ROOT = resolve(here, '..', '..');
export const config = JSON.parse(
  readFileSync(join(ROOT, 'npm', 'publish.config.json'), 'utf8'),
);

export function cargoVersion() {
  const toml = readFileSync(join(ROOT, config.cargoWorkspace), 'utf8');
  const section = toml
    .split(/^\[/m)
    .find((candidate) => candidate.startsWith('workspace.package'));
  const match = section?.match(/^\s*version\s*=\s*"([^"]+)"/m);
  if (!match) {
    throw new Error('could not find [workspace.package] version in Cargo.toml');
  }
  return match[1];
}

// The launcher is scoped, and stays scoped. An unscoped `ostrom` is refused by
// npm's new-name similarity check as too close to `astro` (edit distance 2,
// ~4M weekly downloads), and that check has no override short of a support
// exception. Scoped names bypass it entirely, which is what npm's own 403
// suggests. The package name is not the command: this installs a binary called
// `ostrom` regardless, so the name only appears in the install line.
export function mainPackageName() {
  return `${config.scope}/${config.mainPackage.name}`;
}

export function platformPackageName(platform) {
  return `${config.scope}/${platform.dir}`;
}

export function sourcePlatformDir(platform) {
  return join(ROOT, config.platformDir, platform.dir);
}

export function binaryNames(platform) {
  return config.binaryNames.map((name) => `${name}${platform.ext}`);
}

export function packageDirs(stagingRoot) {
  return [
    ...config.platforms.map((platform) => ({
      dir: join(stagingRoot, platform.dir),
      name: platformPackageName(platform),
      kind: 'platform',
      platform,
    })),
    {
      dir: join(stagingRoot, config.mainPackage.name),
      name: mainPackageName(),
      kind: 'main',
    },
  ];
}

export function requireSafeOutputPath(output) {
  const absolute = resolve(output);
  const targetRoot = join(ROOT, 'target');
  if (!absolute.startsWith(`${targetRoot}${sep}`)) {
    throw new Error(`refusing unsafe output path: ${absolute}`);
  }
  return absolute;
}

export function argValue(args, flag, fallback) {
  const index = args.indexOf(flag);
  if (index < 0) return fallback;
  const value = args[index + 1];
  if (!value || value.startsWith('--')) {
    throw new Error(`${flag} requires a value`);
  }
  return value;
}

// The commit a staged package was built from. It travels with the tarball
// (stage-packages.mjs writes it into package.json before packing) so that
// publish.mjs and wait-for-platforms.mjs can recognise "already published
// from this same source, just a different build" without trusting bytes that
// Rust's non-reproducible release builds cannot guarantee to match (see
// registryState's comment below). Validated wherever it is produced or read
// back, so a truncated or malformed value fails at the point that made it,
// not three steps later as a confusing registry mismatch.
const GIT_HEAD_PATTERN = /^[0-9a-f]{40}$/;

export function requireGitHead(value, source) {
  if (typeof value !== 'string' || !GIT_HEAD_PATTERN.test(value)) {
    throw new Error(
      `invalid git head ${JSON.stringify(value)}${source ? ` (${source})` : ''}; expected 40 lowercase hex characters`,
    );
  }
  return value;
}

export function readManifestGitHead(pkgDir) {
  const manifestPath = join(pkgDir, 'package.json');
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
  if (typeof manifest.gitHead !== 'string') {
    throw new Error(`${manifestPath} has no gitHead; re-run stage-packages.mjs`);
  }
  return requireGitHead(manifest.gitHead, manifestPath);
}

// pack-results.json (verify-pack-output.mjs's output) is the one record of
// what pack.sh actually produced: filename, and the sha512 `integrity` npm
// itself computed over the tarball bytes. Reading it back, rather than
// recomputing a plan from the staging directory, is what lets publish.mjs
// publish exactly those bytes instead of letting `npm publish <dir>`
// re-derive its own tarball at publish time.
export function readPackResults(tarballsRoot) {
  const path = join(tarballsRoot, 'pack-results.json');
  let raw;
  try {
    raw = readFileSync(path, 'utf8');
  } catch (error) {
    throw new Error(
      `could not read ${path}; run npm/scripts/pack.sh first (${error.message})`,
    );
  }
  const results = JSON.parse(raw);
  return new Map(results.map((result) => [result.name, result]));
}

export function tarballIntegrity(tarballPath) {
  const bytes = readFileSync(tarballPath);
  return `sha512-${createHash('sha512').update(bytes).digest('base64')}`;
}

export function verifyTarballIntegrity(tarballPath, expectedIntegrity) {
  const actual = tarballIntegrity(tarballPath);
  if (actual !== expectedIntegrity) {
    throw new Error(
      `${tarballPath} integrity ${actual} does not match its recorded ` +
        `pack-results.json integrity ${expectedIntegrity}; refusing to publish`,
    );
  }
  return actual;
}

// A package this build is ready to publish: the tarball pack.sh actually
// produced and verified (not the staging directory — publishing a directory
// lets `npm publish` re-derive different bytes than what was packed and
// checked into pack-results.json) together with the gitHead
// stage-packages.mjs wrote into its manifest. This pair — integrity and
// gitHead — is the whole identity §2 and §3 compare against the registry.
export function resolvePublishablePackage(pkg, packResults, tarballsRoot) {
  const result = packResults.get(pkg.name);
  if (!result) {
    throw new Error(
      `pack-results.json has no entry for ${pkg.name}; run npm/scripts/pack.sh first`,
    );
  }
  const tarballPath = join(tarballsRoot, result.filename);
  const integrity = verifyTarballIntegrity(tarballPath, result.integrity);
  const gitHead = readManifestGitHead(pkg.dir);
  return { name: pkg.name, tarballPath, integrity, gitHead };
}

// The exact `npm view` invocation used to read registry state, exported so a
// test can pin the flags without spawning npm. `--prefer-online` is not
// optional: npm serves a still-fresh cached packument for a few minutes
// (the registry's own `max-age`), and a wait loop or a post-failure re-read
// would otherwise keep seeing "No match found" for a version that is already
// live — the exact false-absent result this task exists to remove. An
// offline test cannot demonstrate that staleness itself, so it pins the flag
// instead.
export function viewArgs(name, version) {
  return [
    'view',
    `${name}@${version}`,
    'version',
    'dist.integrity',
    'gitHead',
    '--prefer-online',
    '--json',
  ];
}

// npm view --json prints structured JSON on stdout on both success and
// E404 failure (measured on this machine: npm 11.17.0). A non-zero exit
// still carries that JSON on `error.stdout`; anything else (no stdout at
// all — auth failure, network failure, a killed process) is a real
// "unknown" and is left to throw so registryState can retry it.
function defaultView(name, version) {
  try {
    const stdout = execFileSync('npm', viewArgs(name, version), {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    return JSON.parse(stdout);
  } catch (error) {
    if (typeof error.stdout === 'string' && error.stdout.trim().length > 0) {
      return JSON.parse(error.stdout);
    }
    throw error;
  }
}

export const DEFAULT_REGISTRY_STATE_RETRIES = 5;
export const DEFAULT_REGISTRY_STATE_BASE_DELAY_MS = 2000;

export const defaultSleep = (milliseconds) =>
  new Promise((resolveSleep) => setTimeout(resolveSleep, milliseconds));

// What the registry actually holds for <name>@<version>, or the fact that it
// throws rather than guesses.
//
// This replaces registryHasVersion's asymmetric true/false, which returned
// `false` for every failure — a network blip, an auth failure, and a version
// that plainly does not exist all read the same way. That was safe only
// because the one caller of `false` was "publish it", and a redundant publish
// attempt fails loudly on its own if the version really is already there.
// Once a second caller (waiting, and recognising a rebuild-of-tag) needs to
// tell "absent" apart from "unknown", that asymmetry stops being safe: an
// unknown read as absent would make a wait loop declare success, or a
// republish, for a package the registry actually already has under
// different bytes.
//
// So there are exactly three outcomes now. `{ state: 'absent' }` only when
// the view reports npm's own E404 (a missing version and a missing package
// both take this shape — see viewArgs's comment and the v0.14.0 case this
// task starts from). `{ state: 'present', integrity, gitHead }` when the
// view succeeds. Anything else is retried with exponential backoff — default
// 5 retries starting at 2s and doubling — and if it still cannot be
// classified, this throws, naming the package, the version, and the last
// error. An unknown state is never silently treated as absent or present.
export async function registryState(
  name,
  version,
  {
    view = defaultView,
    sleep = defaultSleep,
    retries = DEFAULT_REGISTRY_STATE_RETRIES,
    baseDelayMs = DEFAULT_REGISTRY_STATE_BASE_DELAY_MS,
  } = {},
) {
  let lastError;
  for (let attempt = 0; ; attempt += 1) {
    try {
      const result = view(name, version);
      if (result?.error?.code === 'E404') {
        return { state: 'absent' };
      }
      if (typeof result?.version === 'string') {
        return {
          state: 'present',
          integrity: result['dist.integrity'],
          gitHead: result.gitHead,
        };
      }
      lastError = new Error(
        `unrecognized npm view output for ${name}@${version}: ${JSON.stringify(result)}`,
      );
    } catch (error) {
      lastError = error;
    }
    if (attempt >= retries) {
      throw new Error(
        `could not determine registry state for ${name}@${version} after ` +
          `${retries} retries: ${lastError.message}`,
      );
    }
    await sleep(baseDelayMs * 2 ** attempt);
  }
}

// The one publish/skip/conflict decision, shared by publish.mjs (deciding
// whether to run `npm publish`) and wait-for-platforms.mjs (deciding whether
// a package already published by a sibling job or a previous attempt is
// "there yet") — principle 6: one definition, not two readings of the same
// rule.
//
// npm versions are immutable: once <name>@<version> has bytes on the
// registry, it never gets different bytes. So a present-but-different
// integrity is a real conflict, not a propagation race to wait out — with
// one narrow, explicit exception. Rust release builds are not
// byte-reproducible (measured on this machine: a rebuild from the same tag
// yields different tarball bytes and a different integrity), so recovering
// a partial release by rebuilding the tag will always look like "different
// bytes" even when it is the same source. `--rebuild-of-tag` says to trust
// gitHead identity instead of byte identity for exactly that recovery path.
export function decidePublishAction(
  state,
  { name, version, integrity, gitHead, rebuildOfTag = false },
) {
  if (state.state === 'absent') {
    return { action: 'publish' };
  }
  if (state.integrity === integrity) {
    return {
      action: 'skip',
      message: `already published with identical bytes: ${name}@${version}`,
    };
  }
  if (rebuildOfTag && gitHead && state.gitHead === gitHead) {
    return {
      action: 'skip',
      message: `already published from ${state.gitHead} by an earlier build: ${name}@${version}`,
    };
  }
  const conflict = new Error(
    `${name}@${version}: registry holds different bytes for this version ` +
      `(registry integrity ${state.integrity ?? 'unknown'}, gitHead ${state.gitHead ?? 'unknown'}; ` +
      `local integrity ${integrity ?? 'unknown'}, gitHead ${gitHead ?? 'unknown'})`,
  );
  conflict.code = 'REGISTRY_CONFLICT';
  throw conflict;
}
