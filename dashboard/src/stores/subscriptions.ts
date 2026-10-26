// Subscriptions with their latest quota windows, and the logins the daemon
// found with no subscription (docs/api.md §16). Refreshed on a timer and on
// the events that change them.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import { isMissingEndpoint } from "../api/http";
import type { AgentEvent, DetectedLogin, Subscription } from "../api/types";
import type { EventsStore } from "./events";

/** Events after which the list is stale. */
export const REFRESH_KINDS = ["quota.sample", "quota.blocked", "quota.resumed"];

export function createSubscriptionsStore(api: Pick<Api, "subscriptions">, events?: Pick<EventsStore, "on">) {
  const [list, setList] = createSignal<Subscription[]>([]);
  const [error, setError] = createSignal<unknown>(null);
  const [detected, setDetected] = createSignal<DetectedLogin[]>([]);
  const [detectError, setDetectError] = createSignal<unknown>(null);
  /** The daemon has no subscription endpoints. */
  const [missing, setMissing] = createSignal(false);
  const [loaded, setLoaded] = createSignal(false);
  let timer: ReturnType<typeof setInterval> | undefined;
  let soon: ReturnType<typeof setTimeout> | undefined;

  let inflight: Promise<void> | null = null;
  const load = async () => {
    try {
      setList(await api.subscriptions.list());
      setError(null);
    } catch (e) {
      setError(e);
      if (isMissingEndpoint(e)) {
        // Stop polling an endpoint the daemon does not have.
        setMissing(true);
        clearInterval(timer);
      }
    } finally {
      setLoaded(true);
    }
  };

  /** Always a new request: a refresh after a change must not join an older one. */
  const refresh = (): Promise<void> => {
    const p = load().finally(() => {
      if (inflight === p) inflight = null;
    });
    inflight = p;
    return p;
  };

  /** `GET /api/subscriptions/detect` runs the harness status commands: call it on demand, not on a timer. */
  const detect = async () => {
    try {
      setDetected(await api.subscriptions.detect());
      setDetectError(null);
    } catch (e) {
      setDetectError(e);
    }
  };

  const refreshSoon = (ms = 400) => {
    clearTimeout(soon);
    soon = setTimeout(() => void refresh(), ms);
  };

  const off = events?.on((e: AgentEvent) => {
    if (e.kind === "subscription.offer") void detect();
    if (REFRESH_KINDS.includes(e.kind)) refreshSoon();
  });

  return {
    list,
    error,
    missing,
    loaded,
    detected,
    detectError,
    refresh,
    /** Load the list once, for a page that needs it but does not poll (the agent form). Joins a load in flight. */
    ensureLoaded: (): Promise<void> => (loaded() && !error() ? Promise.resolve() : (inflight ?? refresh())),
    detect,
    byName: (name?: string) => (name ? list().find((s) => s.name === name) : undefined),
    /** Poll every `ms`. Safe to call twice. */
    poll(ms = 15_000) {
      if (timer) return;
      void refresh();
      timer = setInterval(refresh, ms);
    },
    stop() {
      clearInterval(timer);
      clearTimeout(soon);
      timer = undefined;
      off?.();
    },
    replace(s: Subscription) {
      setList((prev) => (prev.some((x) => x.name === s.name) ? prev.map((x) => (x.name === s.name ? s : x)) : [...prev, s]));
    },
    drop(name: string) {
      setList((prev) => prev.filter((x) => x.name !== name));
    },
  };
}

export type SubscriptionsStore = ReturnType<typeof createSubscriptionsStore>;
