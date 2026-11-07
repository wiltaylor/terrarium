// The Threads tab of the Inbox (docs/api.md §6.13): `GET /api/threads`.
// Open lists the threads that are not closed and do not track, the ones that
// wait on the operator first and marked; Tracking lists the threads whose
// agent tracks handed-off work (TER-145); Done lists the closed ones. The project filter
// of the Inbox is the scope. `thread` and `inbox` events read the list again.
// Each row with linked cards has the n/m chip (TER-147): the boards of the
// linked projects load once per list, and a `card` event of one of them
// reads the boards again.

import { createEffect, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import type { Board, Thread } from "../../api/types";
import { Empty, ErrorBanner } from "../../components/ui";
import { api, events, inbox } from "../../stores";
import { ThreadRow } from "./ThreadRow";
import { loadBoards } from "./TrackedCards";
import { inThreadsView, sortThreads, threadListStatus, type ThreadsViewId } from "./threads";

const EMPTY: Record<ThreadsViewId, string> = {
  open: "No open threads. Open one with New thread.",
  tracking: "No thread tracks work. An agent moves its thread here after the handoff.",
  done: "No closed threads.",
};

export function ThreadsView(props: { view: ThreadsViewId; project: string; now: number }) {
  const [list, setList] = createSignal<Thread[]>([]);
  const [loading, setLoading] = createSignal(true);
  const [error, setError] = createSignal<unknown>(null);
  const [boards, setBoards] = createSignal<Record<string, Board | undefined>>({});
  const projectsOf = (rows: Thread[]) => [...new Set(rows.flatMap((t) => (t.cards ?? []).map((c) => c.project)))];
  let boardGen = 0;
  const reloadBoards = async (rows: Thread[]) => {
    const mine = ++boardGen;
    const ps = projectsOf(rows);
    const b = ps.length ? await loadBoards(ps) : {};
    if (mine === boardGen) setBoards(b);
  };
  let gen = 0;
  const reload = async () => {
    const mine = ++gen;
    setLoading(true);
    try {
      const view = props.view;
      const rows = (await api.threads.list({ status: threadListStatus(view), ...(props.project ? { scope: props.project } : {}) })).filter((t) => inThreadsView(view, t));
      if (mine !== gen) return;
      setList(view === "done" ? rows : sortThreads(rows));
      setError(null);
      void reloadBoards(rows);
    } catch (e) {
      if (mine === gen) setError(e);
    } finally {
      if (mine === gen) setLoading(false);
    }
  };
  createEffect(on([() => props.view, () => props.project], () => void reload()));
  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    let tb: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (e.kind === "card" && e.project && projectsOf(list()).includes(e.project)) {
        clearTimeout(tb);
        tb = setTimeout(() => void reloadBoards(list()), 300);
        return;
      }
      if (e.kind !== "thread" && e.kind !== "inbox") return;
      clearTimeout(t);
      t = setTimeout(() => void reload(), 300);
    });
    onCleanup(() => (off(), clearTimeout(t), clearTimeout(tb)));
  });
  return (
    <div class="stack">
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={list().length} fallback={<Empty>{loading() ? "Loading…" : EMPTY[props.view]}</Empty>}>
        <div class="stack" aria-label="Threads">
          <For each={list()}>{(t) => <ThreadRow thread={t} now={props.now} boards={boards()} onChange={() => (void reload(), void inbox.refresh())} />}</For>
        </div>
      </Show>
    </div>
  );
}
