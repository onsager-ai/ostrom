import type { TouchConfig } from "./config.js";

// sprint.jsonl grows without bound and multiple checks each need its full
// contents (trace-lease reads the last record; builder-pass scans backward
// for the last builder pass-ended record). readTrace lets every check ask
// for the file through one context-scoped read instead of each doing its
// own readFileSync, so a doctor run pays that I/O once no matter how many
// checks consult the trace.
export type TraceFile =
  | { exists: false }
  | { exists: true; content: string }
  | { exists: true; error: unknown };

export interface DoctorContext {
  pluginRoot: string;
  configDir: string;
  cwd: string;
  home: string;
  env: NodeJS.ProcessEnv;
  resolveConfig(): TouchConfig;
  readTrace(): TraceFile;
}
