import { useInstance } from "../../instance";
// Home, Blocked (TER-51): every card in the blocked column of every project
// the caller may read, by project, with its link, title, what it needs (the
// first line of the comment of the move to blocked), the agent with its
// status, and how long it has been blocked. It reloads on card and agent
// status events (batched) and each minute, as Work in progress does.

import { A } from "@solidjs/router";
import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import type { ActiveCard } from "../../api/types";
import { Empty, ErrorBanner } from "../../components/ui";

import { cardHref } from "../inbox/questions";
import { assigneeText, byProject, statusTone } from "./active";
import { blockedFor, blockedSummary, reasonLine } from "./blocked";

/** The section. `now` drives the "blocked for" times. */
export function BlockedCards(props: { now: number }) {
  const { api, events } = useInstance();
  const [tick, setTick] = createSignal(0);
  const [cards] = createResource(tick, () => api.cards.active({ columns: ["blocked"] }));
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
    <section aria-label="Blocked" class="blocked-cards">
      <div class="spread">
        <h2>Blocked</h2>
        <Show when={list().length > 0}>
          <span class="small muted">{blockedSummary(list())}</span>
        </Show>
      </div>
      <ErrorBanner error={cards.error} />
      <Show when={list().length > 0} fallback={<Empty>{cards.loading && !cards.latest ? "Loading…" : "Nothing is blocked."}</Empty>}>
        {/* One table for all projects, so the columns line up across the groups. */}
        <div class="table-wrap">
          <table class="blocked-table">
            <colgroup>
              <col class="col-id" />
              <col class="col-title" />
              <col class="col-reason" />
              <col class="col-agent" />
              <col class="col-since" />
            </colgroup>
            <thead class="sr-only">
              <tr>
                <th scope="col">Card</th>
                <th scope="col">Title</th>
                <th scope="col">Needs</th>
                <th scope="col">Agent</th>
                <th scope="col">Blocked for</th>
              </tr>
            </thead>
            <For each={byProject(list())}>
              {(g) => (
                <tbody class="blocked-project">
                  <tr class="blocked-group">
                    <th scope="colgroup" colSpan={5}>
                      <h3>
                        <A href={`/p/${encodeURIComponent(g.project)}/board`}>{g.project}</A>
                      </h3>
                    </th>
                  </tr>
                  <For each={g.cards}>
                    {(c) => {
                      const since = () => blockedFor(c, props.now);
                      return (
                        <tr data-card={c.id}>
                          <td class="mono">
                            <A href={cardHref(c.project, c.id)}>{c.id}</A>
                          </td>
                          <td class="blocked-title" title={c.title}>
                            {c.title}
                          </td>
                          <td class="blocked-reason" title={c.blocked_reason ?? ""}>
                            <Show when={reasonLine(c)} fallback={<span class="muted">no comment</span>}>
                              {(r) => r()}
                            </Show>
                          </td>
                          <td class="blocked-agent">
                            <Show when={c.assignee && c.assignee !== "operator"} fallback={<span class="muted">{assigneeText(c)}</span>}>
                              <A href={`/agents/${encodeURIComponent(c.assignee!)}`}>{assigneeText(c)}</A>
                            </Show>{" "}
                            <Show when={c.assignee_status}>
                              <span class={`chip ${statusTone(c.assignee_status)}`}>{c.assignee_status}</span>
                            </Show>
                          </td>
                          <td class="small" title={c.blocked_since ? `blocked since ${c.blocked_since}` : undefined}>
                            <Show when={since()} fallback={<span class="muted">unknown</span>}>
                              {(s) => <span class="bad">{s()}</span>}
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
