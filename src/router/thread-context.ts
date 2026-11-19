export const THREAD_BODY_EXCERPT_CHARS = 2_000;

export interface ThreadContext {
  kind: "issue" | "pull_request";
  number: number;
  url: string;
  title: string;
  bodyExcerpt: string;
  bodyTruncated: boolean;
}

export function excerptThreadBody(body: string): { bodyExcerpt: string; bodyTruncated: boolean } {
  return {
    bodyExcerpt: body.length > THREAD_BODY_EXCERPT_CHARS ? `${body.slice(0, THREAD_BODY_EXCERPT_CHARS)}…` : body,
    bodyTruncated: body.length > THREAD_BODY_EXCERPT_CHARS,
  };
}
