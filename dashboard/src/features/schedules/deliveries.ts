// The webhook deliveries of one trigger (docs/api.md §19.5):
// `GET /api/schedules/{id}/deliveries`, newest first, a page at a time;
// a redeliver (`POST .../deliveries/{id}/redeliver`, a copy due now); and a
// test send (`POST .../test`, the answer has the delivery and the request).
// The `webhook.delivery` events carry the trigger and the delivery IDs only:
// the panel reads the first page again. No JSX and no effects; the tests
// drive it with the mock API.

import { createSignal } from "solid-js";
import type { Api } from "../../api/client";
import type { AgentEvent, WebhookDelivery, WebhookTestResult } from "../../api/types";
import { applyPage, emptyPaged, type PagedState } from "../../lib/paging";
import { upsertDelivery } from "./webhook";

export const DELIVERIES_PAGE = 10;

/** Is the event about a delivery of this trigger? */
export const touchesDeliveries = (e: Pick<AgentEvent, "kind" | "data">, scheduleId: string): boolean => e.kind === "webhook.delivery" && (e.data as { trigger?: unknown } | undefined)?.trigger === scheduleId;

export function createDeliveries(api: Pick<Api, "schedules">, scheduleId: string, pageSize = DELIVERIES_PAGE) {
  const [state, setState] = createSignal<PagedState<WebhookDelivery>>(emptyPaged<WebhookDelivery>());
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal<string | null>(null);
  const [test, setTest] = createSignal<WebhookTestResult | null>(null);
  let generation = 0;
  const key = (d: WebhookDelivery) => d.id;

  const load = async (reset: boolean) => {
    const gen = reset ? ++generation : generation;
    setLoading(true);
    try {
      const cursor = reset ? undefined : (state().next ?? undefined);
      const page = await api.schedules.deliveries(scheduleId, { limit: pageSize, ...(cursor ? { cursor } : {}) });
      if (gen !== generation) return;
      setState((s) => applyPage(s, page, key, reset));
      setError(null);
    } catch (e) {
      if (gen === generation) setError(e);
    } finally {
      if (gen === generation) setLoading(false);
    }
  };

  return {
    deliveries: () => state().items,
    hasMore: () => state().next !== null,
    initial: () => state().initial,
    loading,
    error,
    /** The delivery that a redeliver or the test waits for, or `test`. */
    busy,
    /** The answer of the last test send. */
    test,
    clearTest: () => setTest(null),
    reload: () => load(true),
    loadMore: () => (state().next ? load(false) : Promise.resolve()),
    /** Read the first page again and merge it on top (an event names only the delivery). */
    async syncHead() {
      if (state().initial) return load(true);
      try {
        const page = await api.schedules.deliveries(scheduleId, { limit: pageSize });
        setState((s) => ({ ...s, items: page.items.reduceRight((list, d) => upsertDelivery(list, d), s.items) }));
        setError(null);
      } catch (e) {
        setError(e);
      }
    },
    /** Send a copy of a delivery; the copy goes on top. */
    async redeliver(id: string): Promise<WebhookDelivery | null> {
      setBusy(id);
      try {
        const d = await api.schedules.redeliver(scheduleId, id);
        setState((s) => ({ ...s, items: upsertDelivery(s.items, d) }));
        setError(null);
        return d;
      } catch (e) {
        setError(e);
        return null;
      } finally {
        setBusy(null);
      }
    },
    /** Send a sample event now; the delivery goes on top and the answer shows. */
    async sendTest(): Promise<WebhookTestResult | null> {
      setBusy("test");
      try {
        const r = await api.schedules.test(scheduleId);
        setTest(r);
        setState((s) => ({ ...s, items: upsertDelivery(s.items, r.delivery) }));
        setError(null);
        return r;
      } catch (e) {
        setError(e);
        return null;
      } finally {
        setBusy(null);
      }
    },
  };
}

export type Deliveries = ReturnType<typeof createDeliveries>;
