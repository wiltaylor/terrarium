// The card side panel (GitHub #31, TER-571): a card key in a question, a card
// body, or a comment opens the card over the current view. One card at a
// time; a new key replaces it.

import { createSignal } from "solid-js";

export interface CardPeekTarget {
  project: string;
  id: string;
}

export function createCardPeekStore() {
  const [current, setCurrent] = createSignal<CardPeekTarget | null>(null);
  return {
    current,
    open: (t: CardPeekTarget) => setCurrent({ project: t.project, id: t.id }),
    close: () => setCurrent(null),
  };
}

export type CardPeekStore = ReturnType<typeof createCardPeekStore>;
