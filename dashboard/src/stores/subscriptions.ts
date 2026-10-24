// Subscriptions with their latest quota windows, refreshed on a timer.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import type { Subscription } from "../api/types";

export function createSubscriptionsStore(api: Pick<Api, "subscriptions">) {
  const [list, setList] = createSignal<Subscription[]>([]);
  const [error, setError] = createSignal<string | null>(null);
  let timer: ReturnType<typeof setInterval> | undefined;

  const refresh = async () => {
    try {
      setList(await api.subscriptions.list());
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  };

  return {
    list,
    error,
    refresh,
    /** Poll every `ms`. Safe to call twice. */
    poll(ms = 15_000) {
      if (timer) return;
      void refresh();
      timer = setInterval(refresh, ms);
    },
    stop() {
      clearInterval(timer);
      timer = undefined;
    },
    replace(s: Subscription) {
      setList((prev) => (prev.some((x) => x.name === s.name) ? prev.map((x) => (x.name === s.name ? s : x)) : [...prev, s]));
    },
  };
}

export type SubscriptionsStore = ReturnType<typeof createSubscriptionsStore>;
