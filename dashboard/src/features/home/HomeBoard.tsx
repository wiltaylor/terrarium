import { useInstance } from "../../instance";
import { A } from "@solidjs/router";
import { createResource, createSignal, For, onCleanup, onMount, Show, type JSX } from "solid-js";
import { allHome, type HomeColumn, type HomeAttention, type HomeCard } from "../../api/home";
import { Empty, ErrorBanner } from "../../components/ui";
import { span } from "../../lib/eta";

import { statusTone } from "./active";
import "./home-board.css";

const columns: { id: HomeColumn; title: string }[] = [
  { id: "in_progress", title: "In progress" }, { id: "review", title: "Review" },
  { id: "accepted", title: "Accepted" }, { id: "blocked", title: "Blocked" },
];

// Solid's For keys by object identity. Keep unchanged rows across API snapshots.
function stableRows<T extends { id: string; project: string }>(rows: T[], previous: T[] = []): T[] {
  return rows.sort((a, b) => a.project.localeCompare(b.project) || a.id.localeCompare(b.id)).map(row =>
    previous.find(old => old.id === row.id && old.project === row.project && JSON.stringify(old) === JSON.stringify(row)) ?? row);
}

export function HomeBoard(props: { now: number; children?: JSX.Element }) {
  const { api, events, projects, cardPeek } = useInstance();
  const [project, setProject] = createSignal("");
  const [tick, setTick] = createSignal(0);
  const [attentionError, setAttentionError] = createSignal<unknown>();
  const [cardsError, setCardsError] = createSignal<unknown>();
  const [attention] = createResource<(HomeAttention & { kind: string })[], number>(tick, async (_, { value }) => {
    try {
    const [monitors, pipelines] = await Promise.all([
      allHome(cursor => api.home.monitors({ cursor })), allHome(cursor => api.home.pipelines({ cursor })),
    ]);
    setAttentionError(undefined);
    return stableRows([...monitors.map(m => ({ ...m, kind: "Monitor" })), ...pipelines.map(p => ({ ...p, kind: "Pipeline" }))], value);
    } catch (error) { setAttentionError(error); return value ?? []; }
  });
  const [cards] = createResource<HomeCard[], readonly [string, number]>(() => [project(), tick()] as const, async ([project], { value }) => {
    try {
      const rows = (await Promise.all(columns.map(c => allHome(cursor => api.home.cards({ column: c.id, project: project || undefined, cursor }))))).flat();
      setCardsError(undefined);
      return stableRows(rows, value);
    } catch (error) { setCardsError(error); return value ?? []; }
  });
  onMount(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const off = events.on(e => {
      if (timer || !(["card", "status", "project"].includes(e.kind) || e.kind.startsWith("monitor.") || e.kind.startsWith("pipeline."))) return;
      timer = setTimeout(() => { timer = undefined; setTick(n => n + 1); }, 2_000);
    });
    const interval = setInterval(() => setTick(n => n + 1), 60_000);
    onCleanup(() => { off(); clearTimeout(timer); clearInterval(interval); });
  });
  return <>
    <section aria-label="System status" class="home-status-area">
      <ErrorBanner error={attentionError()} />
      <Show when={attention.loading && attention.latest === undefined}><p class="muted">Loading status…</p></Show>
      <Show when={!attentionError() && attention.latest?.length === 0}>
        <p class="nominal">All Systems Nominal</p>
      </Show>
      <div class="home-attention">
        <For each={attention.latest ?? []}>{item =>
          <A class="home-status" href={item.link}>
            <span class="small muted">{item.kind} · {item.project}</span>
            <strong>{item.name}</strong><span class={`chip ${item.status === "warn" ? "warn" : "bad"}`}>{item.status}</span>
          </A>
        }</For>
      </div>
    </section>
    {props.children}
    <section aria-label="Work board">
      <div class="spread"><h2>Work board</h2>
        <label>Project <select aria-label="Project" value={project()} onChange={e => setProject(e.currentTarget.value)}>
          <option value="">All projects</option>
          <For each={projects.projects()}>{p => <option value={p.name}>{p.name}</option>}</For>
        </select></label>
      </div>
      <ErrorBanner error={cardsError()} />
      <Show when={cards.loading && cards.latest === undefined}><p class="muted">Loading cards…</p></Show>
      <div class="home-kanban">
        <For each={columns}>{column => <section class="home-lane" aria-label={column.title}>
          <h3>{column.title}</h3>
          <For each={(cards.latest ?? []).filter(c => c.home_column === column.id)} fallback={<Show when={cards.latest !== undefined}><Empty>No cards.</Empty></Show>}>
            {c => <button class="home-card" onClick={() => cardPeek.open({ id: c.id, project: c.project })}>
              <span class="mono small muted">{c.id}</span><strong>{c.title}</strong>
              <span class="row"><span class="chip">{c.project}</span><For each={c.labels}>{label => <span class="chip">{label}</span>}</For></span>
              <Show when={c.assignee}><span>{c.assignee_name ?? c.assignee} <Show when={c.assignee_status}><span class={`chip ${statusTone(c.assignee_status ?? undefined)}`}>{c.assignee_status}</span></Show></span></Show>
              <Show when={c.eta_at}>{at => {
                const left = () => Date.parse(at()) - props.now;
                return <span class={left() < 0 ? "bad" : "muted"}>{left() < 0 ? `overdue ${span(-left(), false)}` : `${span(left(), false)} left`}</span>;
              }}</Show>
              <Show when={column.id === "blocked" && c.blocked_reason}><span class="bad">{c.blocked_reason?.split("\n")[0]}</span></Show>
            </button>}
          </For>
        </section>}</For>
      </div>
    </section>
  </>;
}
