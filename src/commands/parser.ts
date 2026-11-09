import type { CommandName } from "./table.ts";

export type ParsedComment =
  | { kind: "command"; command: Extract<CommandName, "pause" | "resume" | "stop"> }
  | { kind: "redirect" };

/** Fixed commands are recognized only at the start of a comment, ignoring leading whitespace. */
export function parseComment(body: string): ParsedComment {
  const match = body.match(/^\s*@ostrom\s+(pause|resume|stop)\b/i);
  return match === null ? { kind: "redirect" } : { kind: "command", command: match[1]!.toLowerCase() as "pause" | "resume" | "stop" };
}
