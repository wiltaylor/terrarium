// The open questions of the leaders (docs/api.md §6.8), for the sidebar
// badge of the Questions page. A `question` event refreshes them.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import type { Question } from "../api/types";
import { openCounts, sortQuestions } from "../features/questions/model";
import type { EventsStore } from "./events";

export function createQuestionsStore(api: Pick<Api, "questions">, events?: Pick<EventsStore, "on">, opts: { debounceMs?: number } = {}) {
  const [open, setOpen] = createSignal<Question[]>([]);
  const [loaded, setLoaded] = createSignal(false);
  let timer: ReturnType<typeof setTimeout> | undefined;

  const refresh = async () => {
    try {
      setOpen(sortQuestions(await api.questions.list({ status: "open" })));
      setLoaded(true);
    } catch {
      // keep the last list
    }
  };

  events?.on((e) => {
    if (e.kind !== "question") return;
    clearTimeout(timer);
    timer = setTimeout(() => void refresh(), opts.debounceMs ?? 300);
  });

  return {
    /** The open questions, urgent first. */
    open,
    loaded,
    refresh,
    /** The badge: open and urgent counts. */
    counts: () => openCounts(open()),
  };
}

export type QuestionsStore = ReturnType<typeof createQuestionsStore>;
