// A fake GitHub behind a GhRunner, for tests. It serves the REST shapes and query semantics intake
// and the outbox rely on (`since` inclusive on updated_at, sort, per_page/page) and records posts.

import type { GhRunner } from "./gh.ts";

type Item = Record<string, unknown>;

export interface CommentInput {
  login: string | null;
  body: string;
  at: Date;
}

export class FakeGitHub {
  readonly repo = "onsager-ai/ostrom";
  readonly issueComments: Item[] = [];
  readonly reviewComments: Item[] = [];
  readonly issues = new Map<number, Item>();
  readonly reviews = new Map<number, Item[]>();
  readonly issueEvents = new Map<number, Item[]>();
  readonly permissions = new Map<string, string>();
  /** Pull requests served by `pulls?head=`: head branch, number, state. */
  readonly pulls: { branch: string; number: number; state: "open" | "closed" | "merged" }[] = [];
  readonly calls: string[][] = [];
  /** Throw this from the next call whose argv matches. */
  failWhen: ((args: readonly string[]) => Error | undefined) | undefined;
  /** Called after each GET is answered, to change state between pages. */
  afterGet: ((path: string) => void) | undefined;
  #ids = 1000;
  readonly #now: () => Date;

  constructor(now: () => Date) {
    this.#now = now;
  }

  issue(number: number, pr = false): void {
    if (this.issues.has(number)) return;
    this.issues.set(number, {
      number,
      user: { login: "alice" },
      node_id: `${pr ? "PR" : "I"}_${number}`,
      html_url: this.#threadUrl(number, pr),
      updated_at: iso(this.#now()),
      state: "open",
      ...(pr ? { pull_request: { url: `https://api.github.com/repos/${this.repo}/pulls/${number}` } } : {}),
    });
  }

  comment(number: number, input: CommentInput): Item {
    const pr = this.issues.get(number)?.["pull_request"] != null;
    const id = ++this.#ids;
    const item: Item = {
      id,
      node_id: `IC_${id}`,
      html_url: `${this.#threadUrl(number, pr)}#issuecomment-${id}`,
      issue_url: `https://api.github.com/repos/${this.repo}/issues/${number}`,
      ...this.#common(input),
    };
    this.issueComments.push(item);
    this.#touch(number, input.at);
    return item;
  }

  reviewComment(pr: number, input: CommentInput & { reviewId?: number; path?: string; line?: number }): Item {
    const id = ++this.#ids;
    const item: Item = {
      id,
      node_id: `PRRC_${id}`,
      html_url: `${this.#threadUrl(pr, true)}#discussion_r${id}`,
      pull_request_url: `https://api.github.com/repos/${this.repo}/pulls/${pr}`,
      pull_request_review_id: input.reviewId ?? id,
      ...(input.path === undefined ? {} : { path: input.path }),
      ...(input.line === undefined ? {} : { line: input.line }),
      ...this.#common(input),
    };
    this.reviewComments.push(item);
    this.#touch(pr, input.at);
    return item;
  }

  review(pr: number, input: CommentInput & { state: string }): Item {
    const id = ++this.#ids;
    const item: Item = {
      id,
      node_id: `PRR_${id}`,
      html_url: `${this.#threadUrl(pr, true)}#pullrequestreview-${id}`,
      user: input.login === null ? null : { login: input.login },
      body: input.body,
      state: input.state,
      submitted_at: input.state === "PENDING" ? null : iso(input.at),
    };
    this.reviews.set(pr, [...(this.reviews.get(pr) ?? []), item]);
    this.#touch(pr, input.at);
    return item;
  }

  /** Edits a comment, moving its updated_at as GitHub does. */
  edit(item: Item, body: string, at: Date): void {
    item["body"] = body;
    item["updated_at"] = iso(at);
  }

  reopen(number: number, at: Date): void {
    const issue = this.issues.get(number);
    if (issue === undefined) throw new Error(`fake gh: no issue #${number}`);
    issue["state"] = "open";
    issue["updated_at"] = iso(at);
    this.#issueEvent(number, "reopened", at);
  }

  close(number: number, at: Date): void {
    const issue = this.issues.get(number);
    if (issue === undefined) throw new Error(`fake gh: no issue #${number}`);
    issue["state"] = "closed";
    issue["updated_at"] = iso(at);
  }

  commentsOn(number: number): Item[] {
    return this.issueComments.filter((c) => (c["issue_url"] as string).endsWith(`/issues/${number}`));
  }

  readonly gh: GhRunner = async (args) => {
    this.calls.push([...args]);
    const failure = this.failWhen?.(args);
    if (failure) throw failure;
    const [api, flag, method, path, ...rest] = args;
    if (api !== "api" || flag !== "--method" || path === undefined) throw new Error(`fake gh: ${args.join(" ")}`);
    if (method === "POST") return JSON.stringify(this.#post(path, rest));
    if (method === "PATCH") return JSON.stringify(this.#patch(path, rest));
    if (method !== "GET") throw new Error(`fake gh: method ${method}`);
    const body = JSON.stringify(this.#getOne(path) ?? this.#get(path));
    this.afterGet?.(path);
    return body;
  };

  /** One comment or review by id, as GitHub serves it; a missing one fails as `gh` does on a 404. */
  #getOne(path: string): Item | undefined {
    const route = path.slice(`repos/${this.repo}/`.length);
    let match: RegExpMatchArray | null;
    let found: Item | undefined;
    if ((match = route.match(/^collaborators\/([^/]+)\/permission$/))) {
      return permissionBody(this.permissions.get(decodeURIComponent(match[1]!)) ?? "none");
    } else if ((match = route.match(/^issues\/(\d+)$/))) {
      found = this.issues.get(Number(match[1]));
    } else if ((match = route.match(/^issues\/comments\/(\d+)$/))) {
      found = this.issueComments.find((c) => c["id"] === Number(match![1]));
    } else if ((match = route.match(/^pulls\/comments\/(\d+)$/))) {
      found = this.reviewComments.find((c) => c["id"] === Number(match![1]));
    } else if ((match = route.match(/^pulls\/(\d+)\/reviews\/(\d+)$/))) {
      found = this.reviews.get(Number(match[1]))?.find((r) => r["id"] === Number(match![2]));
    } else {
      return undefined;
    }
    if (found === undefined) throw new Error(`gh api failed: gh: Not Found (HTTP 404)`);
    return found;
  }

  #get(path: string): Item[] {
    const url = new URL(`https://api.github.com/${path}`);
    const q = url.searchParams;
    const prefix = `/repos/${this.repo}/`;
    if (!url.pathname.startsWith(prefix)) throw new Error(`fake gh: unknown repo in ${path}`);
    const route = url.pathname.slice(prefix.length);
    const permission = route.match(/^collaborators\/([^/]+)\/permission$/);
    if (permission) return [permissionBody(this.permissions.get(decodeURIComponent(permission[1]!)) ?? "none")];
    let items: Item[];
    let match: RegExpMatchArray | null;
    if (route === "issues/comments") items = this.issueComments;
    else if (route === "pulls/comments") items = this.reviewComments;
    else if (route === "pulls") {
      const head = q.get("head") ?? "";
      return this.pulls
        .filter((pull) => head === `${this.repo.split("/")[0]}:${pull.branch}`)
        .map((pull) => ({
          number: pull.number,
          html_url: this.#threadUrl(pull.number, true),
          state: pull.state === "open" ? "open" : "closed",
          merged_at: pull.state === "merged" ? "2026-11-09T12:00:00Z" : null,
        }));
    } else if (route === "issues") items = [...this.issues.values()];
    else if ((match = route.match(/^issues\/(\d+)\/events$/))) items = this.issueEvents.get(Number(match[1])) ?? [];
    else if ((match = route.match(/^pulls\/(\d+)\/reviews$/))) items = this.reviews.get(Number(match[1])) ?? [];
    else if ((match = route.match(/^issues\/(\d+)\/comments$/))) items = this.commentsOn(Number(match[1]));
    else throw new Error(`fake gh: no route ${route}`);

    const since = q.get("since");
    if (since !== null) items = items.filter((i) => Date.parse(i["updated_at"] as string) >= Date.parse(since));
    if (q.get("sort") === "updated") {
      items = [...items].sort(
        (a, b) =>
          Date.parse(a["updated_at"] as string) - Date.parse(b["updated_at"] as string) ||
          (a["id"] as number) - (b["id"] as number),
      );
    }
    const perPage = Number(q.get("per_page") ?? 30);
    const page = Number(q.get("page") ?? 1);
    return items.slice((page - 1) * perPage, page * perPage);
  }

  #post(path: string, rest: string[]): Item {
    const issue = path.match(new RegExp(`^repos/${this.repo}/issues/(\\d+)/comments$`));
    const reaction = path.match(new RegExp(`^repos/${this.repo}/(issues/comments|pulls/comments)/(\\d+)/reactions$`));
    const [flag, field] = rest;
    if (issue && flag === "-f" && field?.startsWith("body=")) {
      return this.comment(Number(issue[1]), { login: "operator", body: field.slice("body=".length), at: this.#now() });
    }
    if (reaction && flag === "-f" && field?.startsWith("content=")) return { id: ++this.#ids, content: field.slice("content=".length) };
    throw new Error(`fake gh: POST ${path}`);
  }

  #patch(path: string, rest: string[]): Item {
    const match = path.match(new RegExp(`^repos/${this.repo}/issues/comments/(\\d+)$`));
    const [flag, field] = rest;
    if (!match || flag !== "-f" || !field?.startsWith("body=")) throw new Error(`fake gh: PATCH ${path}`);
    const comment = this.issueComments.find((item) => item["id"] === Number(match[1]));
    if (comment === undefined) throw new Error("gh api failed: gh: Not Found (HTTP 404)");
    this.edit(comment, field.slice("body=".length), this.#now());
    return comment;
  }

  #issueEvent(number: number, event: string, at: Date): void {
    const id = ++this.#ids;
    this.issueEvents.set(number, [...(this.issueEvents.get(number) ?? []), {
      id,
      event,
      actor: { login: "alice" },
      created_at: iso(at),
    }]);
  }

  #common(input: CommentInput): Item {
    return {
      user: input.login === null ? null : { login: input.login },
      body: input.body,
      created_at: iso(input.at),
      updated_at: iso(input.at),
    };
  }

  #touch(number: number, at: Date): void {
    const issue = this.issues.get(number);
    if (issue === undefined) throw new Error(`fake gh: no issue #${number}`);
    if (Date.parse(issue["updated_at"] as string) < at.getTime()) issue["updated_at"] = iso(at);
  }

  #threadUrl(number: number, pr: boolean): string {
    return `https://github.com/${this.repo}/${pr ? "pull" : "issues"}/${number}`;
  }
}

/** GitHub's timestamp form: ISO 8601 to the second. */
export function iso(at: Date): string {
  return at.toISOString().replace(/\.\d{3}Z$/, "Z");
}

/**
 * GitHub's collaborator permission answer: `permission` is the legacy level (triage reads as
 * "read", maintain as "write"); `role_name` is the actual role.
 */
function permissionBody(role: string): { permission: string; role_name: string } {
  const legacy: Record<string, string> = { admin: "admin", maintain: "write", write: "write", triage: "read", read: "read" };
  return { permission: legacy[role] ?? "none", role_name: role };
}
