// The outbox is the public boundary for all Worker text, including restored and batched replies.
import { posix } from "node:path";
import type { Store } from "../store/store.ts";

export interface ReplyWorktree {
  worktree: string;
  repo: string;
  headSha?: string;
}

/** Rewrite host paths without interpreting or executing model-written Markdown. */
export function rewritePublicPaths(text: string, contexts: readonly ReplyWorktree[]): string {
  const roots = contexts.map((context) => ({ ...context, worktree: posix.resolve(context.worktree) }))
    .sort((a, b) => b.worktree.length - a.worktree.length);
  function rewrite(path: string, link: boolean): string {
    const location = path.match(/(?::(\d+)|#L(\d+))$/);
    const file = location === null ? path : path.slice(0, location.index);
    const normalized = posix.resolve(file);
    const context = roots.find((root) => normalized.startsWith(`${root.worktree}/`));
    if (context === undefined) return `\`${posix.basename(file).replaceAll("`", "")}\``;
    const relative = posix.relative(context.worktree, normalized);
    if (link && context.headSha !== undefined) {
      const target = relative.split("/").map(encodeURIComponent).join("/");
      return `https://github.com/${context.repo}/blob/${encodeURIComponent(context.headSha)}/${target}${location === null ? "" : `#L${location[1] ?? location[2]}`}`;
    }
    return `\`${relative.replaceAll("`", "")}${location?.[0] ?? ""}\``;
  }
  return text.replace(
    /(!?\[[^\]\n]*\])\(<?(\/[^\n)>]+)>?\)|`(\/[^`\n]+)`|(?<![\w/:.#])\/[^\s`<>"'\[\]()]+/g,
    (match: string, label: string | undefined, target: string | undefined, code: string | undefined) => {
      if (target !== undefined) {
        const rewritten = rewrite(target, true);
        return rewritten.startsWith("https://") ? `${label}(${rewritten})` : rewritten;
      }
      if (code !== undefined) return rewrite(code, false);
      // Sentence punctuation is not part of a bare path.
      const path = match.replace(/[.,;!?]+$/, "");
      return rewrite(path, false) + match.slice(path.length);
    },
  );
}

export function publicReplyBody(store: Store, threadId: number, text: string): string {
  const contexts: ReplyWorktree[] = [];
  for (const ticket of store.ticketsForThread(threadId)) {
    const assignment = store.latestAssignment(ticket.id);
    if (assignment?.worktree == null) continue;
    const candidate = store.mergeCandidateForAssignment(assignment.id);
    contexts.push({
      worktree: assignment.worktree,
      repo: store.ticketContext(ticket.id).source.name,
      ...(candidate === null ? {} : { headSha: candidate.headSha }),
    });
  }
  return rewritePublicPaths(text, contexts);
}
