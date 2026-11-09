// Review replies are intentionally thread-level: a review can contain several inline comments,
// but Ostrom posts one concise summary rather than creating a notification for each one.

export function renderReviewReply(summary: string): string {
  if (summary.trim() === "") throw new Error("a review reply needs a summary");
  return summary;
}
