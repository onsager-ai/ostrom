// Only these approvals carry no Worker instruction. Inline comments always require a turn.
export function isApprovalOnly(state: string, body: string, inlineComments: number): boolean {
  return state === "APPROVED" && inlineComments === 0 &&
    /^(?:(?:lgtm|approved|looks good)[.!]?)?$/i.test(body.trim());
}
