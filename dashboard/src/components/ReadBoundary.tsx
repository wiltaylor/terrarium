import { createEffect, createMemo, createSignal, onMount } from "solid-js";
import { useInstance } from "../instance";

/** A page visit is recorded once, not on event reloads. Keep the old cursor
 * for this opening so live updates cannot move the boundary past the reader. */
export function useReadBoundary(kind: "thread" | "mail" | "card", item: () => string, times: () => string[]) {
  const { api } = useInstance();
  const [previous, setPrevious] = createSignal<string | null>(null);
  const [error, setError] = createSignal<unknown>(null);
  let generation = 0;
  let current = "";
  let visited = false;
  createEffect(() => {
    const id = item();
    const entries = times();
    if (id !== current) {
      current = id;
      visited = false;
      generation++;
      setPrevious(null);
      setError(null);
    }
    if (visited || !entries.length) return;
    visited = true;
    const mine = ++generation;
    const through = new Date(Math.max(...entries.map(Date.parse))).toISOString();
    void Promise.resolve().then(() => api.readPositions.visit(kind, id, through)).then((visit) => {
      if (mine === generation) setPrevious(visit.previous);
    }).catch((e) => { if (mine === generation) setError(e); });
  });
  const first = createMemo(() => firstUnread(times(), previous()));
  return { first, error };
}

export function firstUnread(times: string[], previous: string | null): number {
  return previous === null ? -1 : times.findIndex((at) => Date.parse(at) > Date.parse(previous));
}

export function ReadBoundary() {
  let line!: HTMLDivElement;
  onMount(() => {
    // An explicit permalink takes precedence over the unread position.
    if (!window.location.hash) queueMicrotask(() => line.scrollIntoView?.({ block: "start" }));
  });
  return <div ref={line} role="separator" aria-label="New since your last visit" class="small muted">New since your last visit</div>;
}
