import type { SpendSnapshot } from "./types.ts";

export type SpendLookup =
  | { status: "available"; snapshot: Readonly<SpendSnapshot> }
  | { status: "not_found" | "unavailable"; snapshot: null };

export interface SpendSource {
  getSnapshot(sessionUid: string): Promise<SpendLookup>;
}

export function unavailableSpendSource(): SpendSource {
  return { getSnapshot: async () => ({ status: "unavailable", snapshot: null }) };
}
