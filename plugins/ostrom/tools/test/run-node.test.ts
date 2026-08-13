import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { spawnSync } from "node:child_process";

const pluginRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const shim = join(pluginRoot, "scripts", "run-node.sh");
const roots: string[] = [];

function fakeNode(path: string, label: string): void {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, `#!/bin/sh\nprintf '%s:%s\\n' '${label}' "$1"\n`);
  chmodSync(path, 0o755);
}

afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true });
});

describe("run-node shim", () => {
  it("prints an absolute node path in resolve-only mode", () => {
    const root = mkdtempSync(join(tmpdir(), "ostrom-node-"));
    roots.push(root);
    const node = join(root, "bin", "node");
    fakeNode(node, "unused");

    const result = spawnSync("bash", [shim, "--resolve-only"], {
      env: { HOME: root, PATH: `${dirname(node)}:/usr/bin:/bin` },
      encoding: "utf8",
    });

    expect(result.status).toBe(0);
    expect(result.stderr).toBe("");
    expect(result.stdout).toBe(`${node}\n`);
  });

  it("prefers node already on PATH", () => {
    const root = mkdtempSync(join(tmpdir(), "ostrom-node-"));
    roots.push(root);
    const bin = join(root, "bin");
    fakeNode(join(bin, "node"), "path");

    const result = spawnSync("bash", [shim], {
      env: { HOME: root, PATH: `${bin}:/usr/bin:/bin` },
      encoding: "utf8",
    });

    expect(result.status).toBe(0);
    expect(result.stderr).toBe("");
    expect(result.stdout).toBe(`path:${join(pluginRoot, "dist", "doctor.js")}\n`);
  });

  it("resolves a bare nvm major to the newest installed version", () => {
    const root = mkdtempSync(join(tmpdir(), "ostrom-node-"));
    roots.push(root);
    const nvm = join(root, ".nvm");
    mkdirSync(join(nvm, "alias"), { recursive: true });
    writeFileSync(join(nvm, "alias", "default"), "24\n");
    fakeNode(join(nvm, "versions", "node", "v24.9.0", "bin", "node"), "old");
    fakeNode(
      join(nvm, "versions", "node", "v24.18.0", "bin", "node"),
      "newest",
    );

    const result = spawnSync("bash", [shim], {
      env: { HOME: root, PATH: "/usr/bin:/bin" },
      encoding: "utf8",
    });

    expect(result.status).toBe(0);
    expect(result.stderr).toBe("");
    expect(result.stdout).toBe(
      `newest:${join(pluginRoot, "dist", "doctor.js")}\n`,
    );
  });

  it("fails with one actionable stderr line when no runtime resolves", () => {
    const root = mkdtempSync(join(tmpdir(), "ostrom-node-"));
    roots.push(root);

    const result = spawnSync("bash", [shim], {
      env: {
        HOME: root,
        NVM_DIR: join(root, "missing-nvm"),
        FNM_DIR: join(root, "missing-fnm"),
        VOLTA_HOME: join(root, "missing-volta"),
        ASDF_DATA_DIR: join(root, "missing-asdf"),
        PATH: "/usr/bin:/bin",
        // Neutralise the system-wide fallbacks. Without this the test only
        // passes on a machine that has no node in /usr/local/bin — true of a
        // typical nvm-only dev box, false of every CI runner.
        OSTROM_NODE_FALLBACKS: "",
      },
      encoding: "utf8",
    });

    expect(result.status).not.toBe(0);
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe(
      "ostrom doctor: Node.js 18+ was not found; install Node or set nvm's default alias.\n",
    );
  });
});
