// Pinned conversations (docs/api.md §6.11). The list is on the daemon, so it
// follows the operator to each browser. The open tab and the collapsed side
// bar are per browser (localStorage): a convenience only.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import type { Pin } from "../api/types";
import { activeAfterUnpin, dropPin, isPinned, pickActive } from "../features/pins/model";
import { eventAgentId } from "./agents";
import type { EventsStore } from "./events";

const ACTIVE_KEY = "terrarium.pins.active";
const COLLAPSED_KEY = "terrarium.pins.collapsed";

type KV = Pick<Storage, "getItem" | "setItem" | "removeItem">;

export function createPinsStore(api: Pick<Api, "pins">, events?: Pick<EventsStore, "on">, storage?: KV | null) {
  const read = (k: string) => {
    try {
      return storage?.getItem(k) ?? null;
    } catch {
      return null;
    }
  };
  const write = (k: string, v: string | null) => {
    try {
      if (v === null) storage?.removeItem(k);
      else storage?.setItem(k, v);
    } catch {
      // A full or blocked storage: the value lives in memory only.
    }
  };

  const [pins, setPins] = createSignal<Pin[]>([]);
  const [error, setError] = createSignal<unknown>(null);
  const [wanted, setWanted] = createSignal<string | null>(read(ACTIVE_KEY));
  const [collapsed, setCollapsedSignal] = createSignal(read(COLLAPSED_KEY) === "1");
  // A phone: the side bar is an overlay, closed until the operator opens it.
  const [overlay, setOverlay] = createSignal(false);
  // Each answer carries the whole list; a late answer must not undo a newer one.
  let gen = 0;

  const take = (mine: number, list: Pin[]) => {
    if (mine !== gen) return;
    setPins(list);
    setError(null);
  };

  const select = (id: string | null) => {
    setWanted(id);
    write(ACTIVE_KEY, id);
  };

  const refresh = async () => {
    const mine = ++gen;
    try {
      take(mine, await api.pins.list());
    } catch (e) {
      if (mine === gen) setError(e);
    }
  };

  const pin = async (id: string) => {
    const mine = ++gen;
    try {
      take(mine, await api.pins.pin(id));
      // A new pin opens its tab, and the side bar.
      const added = pins().find((p) => p.agent === id || `${p.project}/${p.name}` === id);
      if (added) select(added.agent);
      setCollapsed(false);
    } catch (e) {
      if (mine === gen) setError(e);
      throw e;
    }
  };

  const unpin = async (id: string) => {
    const before = pins();
    const next = activeAfterUnpin(before, pickActive(before, wanted()), id);
    // At once: the tab goes before the answer comes.
    setPins(dropPin(before, id));
    select(next);
    const mine = ++gen;
    try {
      take(mine, await api.pins.unpin(id));
    } catch (e) {
      if (mine === gen) {
        setError(e);
        void refresh();
      }
      throw e;
    }
  };

  const setCollapsed = (v: boolean) => {
    setCollapsedSignal(v);
    write(COLLAPSED_KEY, v ? "1" : null);
  };

  // The daemon removes the pin of a deleted agent (the foreign key); the tab goes at once.
  const off = events?.on((e) => {
    if (e.kind !== "agent_removed") return;
    const id = eventAgentId(e);
    if (!id || !isPinned(pins(), id)) return;
    const next = activeAfterUnpin(pins(), pickActive(pins(), wanted()), id);
    setPins(dropPin(pins(), id));
    if (wanted() === id) select(next);
  });

  return {
    pins,
    error,
    clearError: () => setError(null),
    /** The agent ULID of the open tab. */
    active: () => pickActive(pins(), wanted()),
    select,
    isPinned: (id: string | undefined) => isPinned(pins(), id),
    refresh,
    pin,
    unpin,
    toggle: (id: string) => (isPinned(pins(), id) ? unpin(id) : pin(id)),
    collapsed,
    setCollapsed,
    overlay,
    setOverlay,
    dispose: () => off?.(),
  };
}

export type PinsStore = ReturnType<typeof createPinsStore>;
