export type CommandName = "redirect" | "pause" | "resume" | "stop" | "close_issue" | "close_pull_request" | "forbidden";

export interface CommandDefinition {
  name: CommandName;
  sayOrDo: string;
  effect: string;
  whoMay: string;
  acknowledgement: string;
}

/** The public command contract. Status cards and Journey 1's table are rendered from this data. */
export const COMMANDS: readonly CommandDefinition[] = [
  {
    name: "redirect",
    sayOrDo: "Any comment on the issue or PR",
    effect: "**Redirect:** steers the same Worker at its next turn boundary",
    whoMay: "Anyone; text from non-collaborators is information, not instruction",
    acknowledgement: "👀; the card shows \"redirected by @x\"",
  },
  {
    name: "pause",
    sayOrDo: "`@ostrom pause`",
    effect: "The Worker finishes its current step, pushes its work in progress, and sleeps. The session is kept.",
    whoMay: "Issue author, or repo triage or higher",
    acknowledgement: "👍; card **Paused**; label `ostrom:paused`",
  },
  {
    name: "resume",
    sayOrDo: "`@ostrom resume`, or removing the label",
    effect: "The same session continues where it stopped",
    whoMay: "Issue author, or repo triage or higher",
    acknowledgement: "👍; card **Working**",
  },
  {
    name: "stop",
    sayOrDo: "`@ostrom stop`",
    effect: "Graceful stop: push work in progress, turn the PR into a draft, post one summary of where it stopped. The ticket ends.",
    whoMay: "Issue author, or repo triage or higher",
    acknowledgement: "👍 and one closing comment",
  },
  {
    name: "close_issue",
    sayOrDo: "Close the issue",
    effect: "Same as stop. The branch is kept; reopening the issue asks the Router again.",
    whoMay: "Whoever can close it",
    acknowledgement: "Card **Stopped: issue closed**",
  },
  {
    name: "close_pull_request",
    sayOrDo: "Close the PR",
    effect: "Ends that attempt only; the issue stays open for a new decision",
    whoMay: "Whoever can close it",
    acknowledgement: "Card updated, no new comment",
  },
  {
    name: "forbidden",
    sayOrDo: "A command from someone without permission",
    effect: "Ignored",
    whoMay: "",
    acknowledgement: "😕 only; no comment, so no noise and no argument",
  },
];

export function command(name: CommandName): CommandDefinition {
  return COMMANDS.find((definition) => definition.name === name)!;
}

export function renderCommandTable(): string {
  return [
    "| Say or do | Effect | Who may | Acknowledged by |",
    "|---|---|---|---|",
    ...COMMANDS.map((definition) =>
      `| ${definition.sayOrDo} | ${definition.effect} |${cell(definition.whoMay)}| ${definition.acknowledgement} |`,
    ),
  ].join("\n");
}

function cell(value: string): string {
  return value === "" ? " " : ` ${value} `;
}

export function controlsLine(): string {
  return "Controls: `@ostrom pause`, `@ostrom resume`, `@ostrom stop`, or reply to redirect.";
}
