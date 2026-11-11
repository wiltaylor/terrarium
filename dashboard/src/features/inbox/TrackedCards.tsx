import { useInstance } from "../../instance";
// The linked cards of an Inbox thread on its page (TER-147, M1-M3): a mini
// kanban, one row per project with that project's own board columns in board
// order. A tile shows project:ID, the title, the assignee, the ETA, and the
// state chip (TER-158); a click opens the card on its board. The panel is open
// at the top while the thread tracks, and folded otherwise. The cards come
// live from the boards: a `card` event of a linked project reads them again.
//
// Card titles and names are untrusted: text nodes only.

import { A } from "@solidjs/router";
import { createEffect, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import type { Board, CardSummary as Card, Thread, ThreadCard } from "../../api/types";
import { cardState } from "../../lib/cardState";
import { countdown } from "../../lib/eta";

import { cardHref } from "./questions";
import { cardProgress, kanbanRows, linkedProjects, progressChip } from "./trackedCards";

/** The boards of `projects`; a board that does not load is absent (no read right, or an error). */
export async function loadBoards(projects: readonly string[], links: readonly ThreadCard[] = []): Promise<Record<string, Board | undefined>> {
  const { api } = useInstance();
  const got = await Promise.allSettled(projects.map((p) => api.board.get(p)));
  const out: Record<string, Board | undefined> = {};
  projects.forEach((p, i) => {
    const r = got[i]!;
    out[p] = r.status === "fulfilled" ? r.value : undefined;
  });
  // Board lists omit Done. Load only missing linked cards, not the entire
  // completed history of each project, so progress counts remain correct.
  const missing = new Map(links.filter(l => out[l.project] && !out[l.project]!.cards.some(c => c.id === l.card)).map(l => [l.card, l]));
  await Promise.all([...missing.values()].map(async l => {
    try {
      const card = await api.cards.get(l.card);
      if (card.project === l.project && !card.archived) out[l.project]!.cards.push(card);
    } catch { /* A deleted or inaccessible card remains missing. */ }
  }));
  return out;
}

/** The n/m chip of a thread: the linked cards in review or later, of all linked cards on their boards. */
export function ProgressChip(props: { thread: Thread; boards: Record<string, Board | undefined> }) {
  const chip = () => progressChip(cardProgress(props.thread.cards ?? [], props.boards));
  return (
    <Show when={chip()}>
      {(c) => (
        <span class={`chip${c().complete ? " ok" : " info"} thread-progress`} data-testid="thread-progress" title={c().title} aria-label={c().title}>
          {c().label}
        </span>
      )}
    </Show>
  );
}

/** The mini kanban of the linked cards; nothing when the thread links no card. */
export function TrackedCards(props: { thread: Thread; now: number }) {
  const { events } = useInstance();
  const links = () => props.thread.cards ?? [];
  const projects = () => linkedProjects(links());
  const [boards, setBoards] = createSignal<Record<string, Board | undefined>>({});
  const [loaded, setLoaded] = createSignal(false);
  let gen = 0;
  const load = async () => {
    const mine = ++gen;
    const ps = projects();
    if (!ps.length) return;
    const b = await loadBoards(ps, links());
    if (mine !== gen) return;
    setBoards(b);
    setLoaded(true);
  };
  createEffect(on(() => links().map(l => `${l.project}:${l.card}`).join("\n"), () => void load()));
  onMount(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (e.kind !== "card" || !e.project || !projects().includes(e.project)) return;
      clearTimeout(timer);
      timer = setTimeout(() => void load(), 300);
    });
    onCleanup(() => (off(), clearTimeout(timer)));
  });
  const rows = () => kanbanRows(links(), boards());
  const tracking = () => props.thread.status === "tracking";
  return (
    <Show when={links().length}>
      <details class="card-box tracked-cards" open={tracking()} aria-label="Linked cards" data-open={tracking() ? "tracking" : "folded"}>
        <summary class="row small" style={{ gap: "8px" }}>
          <strong>Linked cards</strong>
          <span class="muted">
            {links().length} {links().length === 1 ? "card" : "cards"} in {projects().length} {projects().length === 1 ? "project" : "projects"}
          </span>
          <ProgressChip thread={props.thread} boards={boards()} />
        </summary>
        <Show when={loaded()} fallback={<span class="small muted">Loading the boards…</span>}>
          <div class="stack" style={{ gap: "10px" }}>
            <For each={rows()}>
              {(row) => (
                <section class="tracked-row" aria-label={`Project ${row.project}`} data-project={row.project}>
                  <div class="row small" style={{ gap: "8px" }}>
                    <A href={`/p/${encodeURIComponent(row.project)}/board`}>
                      <strong>{row.project}</strong>
                    </A>
                    <Show when={row.unavailable}>
                      <span class="muted">The board did not load.</span>
                    </Show>
                    <Show when={!row.unavailable && row.missing.length}>
                      <span class="muted" title={row.missing.join(", ")}>
                        {row.missing.length} archived or not on the board
                      </span>
                    </Show>
                  </div>
                  <div class="tracked-columns">
                    <For each={row.columns}>
                      {(col) => (
                        <div class={`tracked-col${col.cards.length ? "" : " empty"}`} data-column={col.name} title={col.cards.length ? undefined : `${col.title}: no linked card`}>
                          <div class="tracked-col-head small muted">
                            <span class="tracked-col-title">{col.title}</span>
                            <Show when={col.cards.length}>
                              <span> {col.cards.length}</span>
                            </Show>
                          </div>
                          <For each={col.cards}>{(c) => <Tile card={c} now={props.now} />}</For>
                        </div>
                      )}
                    </For>
                  </div>
                </section>
              )}
            </For>
          </div>
        </Show>
      </details>
    </Show>
  );
}

/** One linked card: project:ID, title, assignee, ETA, and the state chip. A click opens it on its board. */
function Tile(props: { card: Card; now: number }) {
  const c = () => props.card;
  const state = () => cardState(c());
  const eta = () => countdown(c(), props.now);
  return (
    <A class="kcard tracked-tile" href={cardHref(c().project, c().id)} data-card={c().id} title={`Open card ${c().id} on the ${c().project} board`}>
      <div class="spread small">
        <span class="mono muted">
          {c().project}:{c().id}
        </span>
        <Show when={state()}>
          {(s) => (
            <span class={`chip ${s().tone} card-state`} data-state={s().kind} title={s().title}>
              {s().label}
            </span>
          )}
        </Show>
      </div>
      <div class="title">{c().title}</div>
      <div class="spread small">
        <span class="muted">{c().assignee ? (c().assignee_name ?? c().assignee) : "unassigned"}</span>
        <Show when={eta()}>
          {(e) => (
            <span class={`chip${e().overdue ? " bad" : " info"}`} role="timer" title={e().title}>
              {e().label}
            </span>
          )}
        </Show>
      </div>
    </A>
  );
}
