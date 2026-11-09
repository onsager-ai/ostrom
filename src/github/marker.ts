// Ostrom's hidden reply marker. Replies go out through the operator's own `gh` login, so intake
// cannot tell Ostrom's comments apart by actor; it tells them apart by this marker instead
// (docs/direction.md, open question 3, option A). The id also lets the outbox find a reply it
// already posted.

export const REPLY_MARKER_PREFIX = "<!-- ostrom:reply id=";

/** Outbox ids are embedded in an HTML comment, so they are limited to characters that cannot end it. */
const REPLY_ID = /^[A-Za-z0-9._:-]+$/;

export function replyMarker(replyId: string): string {
  if (!REPLY_ID.test(replyId)) throw new Error(`outbox id ${JSON.stringify(replyId)} cannot go in a marker`);
  return `${REPLY_MARKER_PREFIX}${replyId} -->`;
}

/** True for any body carrying a marker: Ostrom's own reply, which intake drops. */
export function hasReplyMarker(body: string): boolean {
  return body.includes(REPLY_MARKER_PREFIX);
}
