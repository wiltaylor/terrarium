// Home, Work in progress (TER-43): every card in a working or research
// column of every project, with its project, column, agent and live
// status, and ETA. It reloads on card and agent status events (batched)
// and each minute. All projects share one table with one column template,
// so the columns line up across the project groups (TER-52).

import { A } from "@solidjs/router";
import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import type { ActiveCard } from "../../api/types";
import { Empty, ErrorBanner } from "../../components/ui";
import { api, events } from "../../stores";
import { cardHref } from "../inbox/questions";
import { assigneeText, byProject, columnLabel, etaText, statusTone, summary } from "./active";
import { idleBadge } from "../../lib/idle";

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
        {/* One table for all projects, so the columns line up across the groups (TER-52). */}
        <div class="table-wrap">
          <table class="active-table">
            <colgroup>
              <col class="col-id" />
              <col class="col-title" />
              <col class="col-column" />
              <col class="col-agent" />
              <col class="col-eta" />
            </colgroup>
            <thead class="sr-only">
              <tr>
                <th scope="col">Card</th>
                <th scope="col">Title</th>
                <th scope="col">Column</th>
                <th scope="col">Agent</th>
                <th scope="col">ETA</th>
              </tr>
            </thead>
            <For each={byProject(list())}>
              {(g) => (
                <tbody class="active-project">
                  <tr class="active-group">
                    <th scope="colgroup" colSpan={5}>
                      <h3>
                        <A href={`/p/${encodeURIComponent(g.project)}/board`}>{g.project}</A>
                      </h3>
                    </th>
                  </tr>
                  <For each={g.cards}>
                    {(c) => {
                      const eta = () => etaText(c, props.now);
                      return (
                        <tr data-card={c.id}>
                          <td class="mono">
                            <A href={cardHref(c.project, c.id)}>{c.id}</A>
                          </td>
                          <td class="active-title" title={c.title}>
                            {c.title}
                          </td>
                          <td>
                            <span class="chip">{columnLabel(c.column_kind)}</span>
                          </td>
                          <td class="active-agent">
                            <Show when={c.assignee && c.assignee !== "operator"} fallback={<span class="muted">{assigneeText(c)}</span>}>
                              <A href={`/agents/${encodeURIComponent(c.assignee!)}`}>{assigneeText(c)}</A>
                            </Show>{" "}
                            <Show when={c.assignee_status}>
                              <span class={`chip ${statusTone(c.assignee_status)}`}>{c.assignee_status}</span>
                            </Show>
                            <Show when={idleBadge(c, props.now)}>
                              {(b) => (
                                <>
                                  {" "}
                                  <span class="chip warn idle-badge" title={b().title} aria-label={b().title}>
                                    {b().label}
                                  </span>
                                </>
                              )}
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
              )}
            </For>
          </table>
        </div>
      </Show>
    </section>
  );
}
