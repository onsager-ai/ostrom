import type { CommandName } from "./table.ts";

export type ParsedComment =
  | { kind: "command"; command: Extract<CommandName, "pause" | "resume" | "stop"> }
  | { kind: "command"; command: "use" | "effort"; value: string }
  | { kind: "redirect" };

/** Selection commands anywhere in a comment, used when that comment starts a ticket. */
export function parseSelectionCommands(body: string): Extract<ParsedComment, { command: "use" | "effort" }>[] {
  return [...body.matchAll(/@ostrom\s+(use|effort)\b\s*(\S*)/gi)].map((match) => ({
    kind: "command",
    command: match[1]!.toLowerCase() as "use" | "effort",
    value: match[2]!,
  }));
}

/** Fixed commands are recognized only at the start of a comment, ignoring leading whitespace. */
export function parseComment(body: string): ParsedComment {
  const selection = body.match(/^\s*@ostrom\s+(use|effort)\b\s*(\S*)/i);
  if (selection) return { kind: "command", command: selection[1]!.toLowerCase() as "use" | "effort", value: selection[2]! };
  const match = body.match(/^\s*@ostrom\s+(pause|resume|stop)\b/i);
  return match === null ? { kind: "redirect" } : { kind: "command", command: match[1]!.toLowerCase() as "pause" | "resume" | "stop" };
}
