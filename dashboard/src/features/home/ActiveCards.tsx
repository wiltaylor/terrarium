// Home, Work in progress (TER-43): every card in a working or research
// column of every project, with its project, column, agent and live
// status, and ETA. It reloads on card and agent status events (batched)
// and each minute.

import { A } from "@solidjs/router";
import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import type { ActiveCard } from "../../api/types";
import { Empty, ErrorBanner } from "../../components/ui";
import { api, events } from "../../stores";
import { cardHref } from "../inbox/questions";
import { assigneeText, byProject, columnLabel, etaText, statusTone, summary } from "./active";

/** The section. `now` drives the ETA countdown. */
export function ActiveCards(props: { now: number }) {
  const [tick, setTick] = createSignal(0);
  const [cards] = createResource(tick, () => api.cards.active());
  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (t || (e.kind !== "card" && e.kind !== "status")) return;
      t = setTimeout(() => {
        t = undefined;
        setTick((n) => n + 1);
      }, 2_000);
    });
    const every = setInterval(() => setTick((n) => n + 1), 60_000);
    onCleanup(() => {
      off();
      clearTimeout(t);
      clearInterval(every);
    });
  });
  const list = (): ActiveCard[] => cards.latest ?? [];
  return (
    <section aria-label="Work in progress" class="active-cards">
      <div class="spread">
        <h2>Work in progress</h2>
        <Show when={list().length > 0}>
          <span class="small muted">{summary(list())}</span>
        </Show>
      </div>
      <ErrorBanner error={cards.error} />
      <Show
        when={list().length > 0}
        fallback={<Empty>{cards.loading && !cards.latest ? "Loading…" : "No card is in progress or in research."}</Empty>}
      >
        <For each={byProject(list())}>
          {(g) => (
            <div class="active-project">
              <h3>
                <A href={`/p/${encodeURIComponent(g.project)}/board`}>{g.project}</A>
              </h3>
              <table>
                <tbody>
                  <For each={g.cards}>
                    {(c) => {
                      const eta = () => etaText(c, props.now);
                      return (
                        <tr data-card={c.id}>
                          <td class="mono">
                            <A href={cardHref(c.project, c.id)}>{c.id}</A>
                          </td>
                          <td>{c.title}</td>
                          <td>
                            <span class="chip">{columnLabel(c.column_kind)}</span>
                          </td>
                          <td>
                            <Show when={c.assignee && c.assignee !== "operator"} fallback={<span class="muted">{assigneeText(c)}</span>}>
                              <A href={`/agents/${encodeURIComponent(c.assignee!)}`}>{assigneeText(c)}</A>
                            </Show>{" "}
                            <Show when={c.assignee_status}>
                              <span class={`chip ${statusTone(c.assignee_status)}`}>{c.assignee_status}</span>
                            </Show>
                          </td>
                          <td class="small">
                            <Show when={eta()} fallback={<span class="muted">no ETA</span>}>
                              {(e) => <span class={e().overdue ? "bad" : "muted"}>{e().label}</span>}
                            </Show>
                          </td>
                        </tr>
                      );
                    }}
                  </For>
                </tbody>
              </table>
            </div>
          )}
        </For>
      </Show>
    </section>
  );
}
