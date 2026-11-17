import type { Store } from "../store/store.ts";
import { discoverModels, type DiscoveredModel, type Harness } from "./models.ts";

export type Discover = (harness: Harness, env: Record<string, string | undefined>, signal?: AbortSignal) => Promise<DiscoveredModel[]>;

/** Serialize refreshes so an older response cannot overwrite a newer catalog. */
export function modelDiscovery(store: Store, env: Record<string, string | undefined>, log: (message: string) => void,
  discover: Discover = discoverModels) {
  const controller = new AbortController();
  let pending: Promise<ReturnType<Store["models"]["view"]>> | undefined;
  async function refreshAll() {
    await Promise.all((["claude", "codex"] as const).map(async (harness) => {
      try {
        const models = await discover(harness, env, controller.signal);
        if (!controller.signal.aborted) store.models.discovered(harness, models);
      } catch (error) {
        if (controller.signal.aborted) return;
        const message = error instanceof Error ? error.message : String(error);
        store.models.failed(harness, message);
        log(`${harness} model discovery failed: ${message}`);
      }
    }));
    return store.models.view();
  }
  return {
    refresh() {
      pending ??= refreshAll().finally(() => { pending = undefined; });
      return pending;
    },
    async close() { controller.abort(); await pending; },
  };
}
