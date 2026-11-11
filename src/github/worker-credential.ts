import { spawn } from "node:child_process";
import { GitHubApp, loadAppConfig } from "./app.ts";

export async function run(mode: "gh" | "git", executable = "gh"): Promise<void> {
  try {
    if (mode === "git" && process.argv.at(-1) !== "get") return;
    const config = loadAppConfig(process.env);
    const repo = process.env["OSTROM_REPO"];
    if (!config || !repo || !/^[\w.-]+\/[\w.-]+$/.test(repo)) throw new Error("Missing GitHub App Worker configuration");
    const app = new GitHubApp(config, repo);
    if (mode === "git") {
      let input = "";
      for await (const chunk of process.stdin) input += String(chunk);
      const fields = Object.fromEntries(input.trim().split("\n").map((line) => {
        const separator = line.indexOf("=");
        return [line.slice(0, separator), line.slice(separator + 1)];
      }));
      if (fields["protocol"] !== "https" || fields["host"] !== "github.com" || fields["path"]?.replace(/\.git$/, "") !== repo) throw new Error("GitHub App credential requested for a different repository");
      // This is Git's private credential protocol, never a log or saved credential.
      process.stdout.write(`username=x-access-token\npassword=${await app.token()}\n\n`);
      return;
    }
    const child = spawn(executable, process.argv.slice(2), { env: { ...process.env, GH_TOKEN: await app.token() }, stdio: "inherit" });
    process.exitCode = await new Promise<number>((resolve, reject) => {
      child.once("error", reject);
      child.once("exit", (code) => resolve(code ?? 1));
    });
  } catch {
    process.stderr.write("GitHub App authentication or command failed\n");
    process.exitCode = 1;
  }
}

if (import.meta.main) await run("git");
