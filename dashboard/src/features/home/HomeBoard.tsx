import { A } from "@solidjs/router";
import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { allHome, type HomeColumn } from "../../api/home";
import { Empty, ErrorBanner } from "../../components/ui";
import { span } from "../../lib/eta";
import { api, cardPeek, events, projects } from "../../stores";
import { statusTone } from "./active";
import "./home-board.css";

const columns: { id: HomeColumn; title: string }[] = [
  { id: "in_progress", title: "In progress" }, { id: "review", title: "Review" },
  { id: "accepted", title: "Accepted" }, { id: "blocked", title: "Blocked" },
];

export function HomeBoard(props: { now: number }) {
  const [project, setProject] = createSignal("");
  const [tick, setTick] = createSignal(0);
  const [attention] = createResource(tick, async () => {
    const [monitors, pipelines] = await Promise.all([
      allHome(cursor => api.home.monitors({ cursor })), allHome(cursor => api.home.pipelines({ cursor })),
    ]);
    return [...monitors.map(m => ({ ...m, kind: "Monitor" })), ...pipelines.map(p => ({ ...p, kind: "Pipeline" }))];
  });
  const [cards] = createResource(() => [project(), tick()] as const, async ([project]) =>
    (await Promise.all(columns.map(c => allHome(cursor => api.home.cards({ column: c.id, project: project || undefined, cursor }))))).flat());
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
    <section aria-label="Needs attention">
      <h2>Needs attention</h2>
      <ErrorBanner error={attention.error} />
      <Show when={attention.loading}><p class="muted">Loading status…</p></Show>
      <Show when={!attention.error && !attention.loading && attention.latest?.length === 0}>
        <p class="nominal">All Systems Nominal</p>
      </Show>
      <div class="home-attention">
        <For each={attention.error ? [] : attention.latest ?? []}>{item =>
          <A class="home-status" href={item.link}>
            <span class="small muted">{item.kind} · {item.project}</span>
            <strong>{item.name}</strong><span class={`chip ${item.status === "warn" ? "warn" : "bad"}`}>{item.status}</span>
          </A>
        }</For>
      </div>
    </section>
    <section aria-label="Work board">
      <div class="spread"><h2>Work board</h2>
        <label>Project <select aria-label="Project" value={project()} onChange={e => setProject(e.currentTarget.value)}>
          <option value="">All projects</option>
          <For each={projects.projects()}>{p => <option value={p.name}>{p.name}</option>}</For>
        </select></label>
      </div>
      <ErrorBanner error={cards.error} />
      <Show when={cards.loading}><p class="muted">Loading cards…</p></Show>
      <div class="home-kanban">
        <For each={columns}>{column => <section class="home-lane" aria-label={column.title}>
          <h3>{column.title}</h3>
          <For each={cards.error || cards.loading ? [] : (cards.latest ?? []).filter(c => c.home_column === column.id)} fallback={<Show when={!cards.loading && !cards.error}><Empty>No cards.</Empty></Show>}>
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
