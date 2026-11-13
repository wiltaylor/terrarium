import { InstanceAnchor } from "../../instance/navigation";
import { useInstance } from "../../instance";
// The Calendar page (docs/api.md §33): the calendar of a project
// (/p/:project/calendar), with the global events marked, and the combined
// view of every project for the operator (/calendar). A month grid, a week
// of day columns, and an agenda list; a phone gets the agenda first. An
// occurrence opens the event: its facts, the description, Edit, Remove, and
// Skip for one occurrence of a repeating event. A viewer without the write
// right (`can_write`, `can_edit`) reads only. The page loads again on each
// `calendar.*` event.

import { useParams } from "@solidjs/router";
import { createEffect, createMemo, createResource, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import type { CalendarEvent, CalendarOccurrence, CalendarView } from "../../api/types";
import { Markdown } from "../../components/Markdown";
import { ConfirmButton, Empty, ErrorBanner, Modal, PageHead, Tabs, useNow } from "../../components/ui";
import { renderMarkdown } from "../../lib/markdown";

import { EventDialog } from "./EventDialog";
import { agenda, DAY_LABELS, dayKey, isRunning, kindClass, kindLabel, monthGrid, monthRange, scopeLabel, timeLabel, weekDays, weekStart } from "./model";
import { DAYS } from "./recur";

export type ViewMode = "month" | "week" | "agenda";
const MODES: { id: ViewMode; label: string }[] = [
  { id: "month", label: "Month" },
  { id: "week", label: "Week" },
  { id: "agenda", label: "Agenda" },
];
const PHONE = "(max-width: 760px)";
const DAY = 86_400_000;

function Chip(props: { o: CalendarOccurrence; now: number; showScope: boolean; onOpen: (o: CalendarOccurrence) => void }) {
  return (
    <button
      type="button"
      class={`cal-chip ${kindClass(props.o.kind)}`}
      classList={{ "cal-global": props.o.global, "cal-live": isRunning(props.o, props.now) }}
      title={`${kindLabel(props.o.kind)}: ${props.o.title} (${timeLabel(props.o)})${props.o.global ? ", global" : ""}`}
      onClick={() => props.onOpen(props.o)}
    >
      <span class="cal-chip-time">{props.o.all_day ? "" : timeLabel(props.o).split("–")[0]}</span>
      <span class="cal-chip-title">{props.o.title}</span>
      <Show when={props.showScope || props.o.global}>
        <span class="cal-chip-scope">{scopeLabel(props.o)}</span>
      </Show>
    </button>
  );
}

function EventDetail(props: { event: CalendarEvent; occurrence: CalendarOccurrence; now: number; onClose: () => void; onEdit: () => void; onChanged: () => void }) {
  const { api } = useInstance();
  const [error, setError] = createSignal<unknown>(null);
  const e = () => props.event;
  const act = async (fn: () => Promise<unknown>) => {
    setError(null);
    try {
      await fn();
      props.onChanged();
    } catch (x) {
      setError(x);
    }
  };
  const links = () => Object.entries(e().links).filter(([, v]) => v) as [string, string][];
  return (
    <Modal title={e().title} onClose={() => props.onClose()} class="modal-wide">
      <div class="stack">
        <div class="row" style={{ "flex-wrap": "wrap", gap: "8px" }}>
          <span class={`cal-badge ${kindClass(e().kind)}`}>{kindLabel(e().kind)}</span>
          <Show when={e().global}>
            <span class="chip info">global</span>
          </Show>
          <Show when={isRunning(props.occurrence, props.now)}>
            <span class="chip">now</span>
          </Show>
          <Show when={!e().can_edit}>
            <span class="muted small">{e().global ? "Global events are read-only here; a sysadmin or the operator changes them." : "Read-only: ask a lead to change it."}</span>
          </Show>
        </div>
        <dl class="pl-facts">
          <dt>This occurrence</dt>
          <dd>
            {new Date(props.occurrence.start).toLocaleString()} ({timeLabel(props.occurrence)})
          </dd>
          <dt>Zone</dt>
          <dd>{e().tz}</dd>
          <Show when={e().repeat}>
            <dt>Repeats</dt>
            <dd>
              {e().repeat} <span class="muted mono small">{e().rrule}</span>
            </dd>
          </Show>
          <Show when={e().exceptions.length}>
            <dt>Skipped</dt>
            <dd>{e().exceptions.map((x) => new Date(x).toLocaleString()).join(", ")}</dd>
          </Show>
          <dt>Calendar</dt>
          <dd>{e().project ?? "global"}</dd>
          <dt>Assignees</dt>
          <dd>{e().assignees.map((a) => `${a.project}/${a.name}`).join(", ") || "none"}</dd>
          <Show when={links().length}>
            <dt>Links</dt>
            <dd>
              <For each={links()}>
                {([k, v]) => (
                  <span class="cal-link">
                    {k}:{" "}
                    <Show when={k === "url" && /^https?:\/\//i.test(v)} fallback={<span class="mono">{v}</span>}>
                      <InstanceAnchor href={v} target="_blank" rel="noopener noreferrer">
                        {v}
                      </InstanceAnchor>
                    </Show>
                  </span>
                )}
              </For>
            </dd>
          </Show>
          <dt>Made by</dt>
          <dd>
            {e().created_by_name}, {new Date(e().created_at).toLocaleString()}
          </dd>
        </dl>
        <Show when={e().description.trim()}>
          <div class="wiki-body cal-desc">
            <Markdown html={renderMarkdown(e().description, e().project ?? "")} project={e().project ?? undefined} />
          </div>
        </Show>
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <Show when={e().can_edit}>
          <div class="row" style={{ "justify-content": "flex-end", "flex-wrap": "wrap" }}>
            <Show when={e().recurrence}>
              <ConfirmButton label="Skip this occurrence" confirmLabel="Skip it" onConfirm={() => act(() => api.calendar.skip(e().id, props.occurrence.start))} />
            </Show>
            <ConfirmButton label={e().recurrence ? "Remove every occurrence" : "Remove"} confirmLabel="Remove" class="danger" onConfirm={() => act(async () => (await api.calendar.remove(e().id), props.onClose()))} />
            <button class="primary" onClick={() => props.onEdit()}>
              Edit
            </button>
          </div>
        </Show>
      </div>
    </Modal>
  );
}

export default function CalendarPage() {
  const params = useParams<{ project?: string }>();
  return <Show when={{ project: params.project ?? null }} keyed>{(scope) => <CalendarBody project={scope.project} legacy />}</Show>;
}

export function CalendarPanel(props: { project: string; view?: ViewMode; onViewChange?: (view: ViewMode) => void; onSelectionChange?: (occurrence: CalendarOccurrence | null) => void }) {
  return <Show when={props.project} keyed>{(project) => <CalendarBody project={project} view={props.view} onViewChange={props.onViewChange} onSelectionChange={props.onSelectionChange} />}</Show>;
}

function CalendarBody(props: { project: string | null; legacy?: boolean; view?: ViewMode; onViewChange?: (view: ViewMode) => void; onSelectionChange?: (occurrence: CalendarOccurrence | null) => void }) {
  const { api, events } = useInstance();
  const project = () => props.project;
  const now = useNow(30_000);
  const phone = typeof matchMedia === "function" ? matchMedia(PHONE) : undefined;
  const [mode, updateMode] = createSignal<ViewMode>(props.view ?? (props.legacy && phone?.matches ? "agenda" : "month"));
  const setMode = (value: ViewMode) => { updateMode(value); props.onViewChange?.(value); };
  createEffect(on(() => props.view, (value) => { if (value) updateMode(value); }));
  const [anchor, setAnchor] = createSignal(Date.now());
  const [filter, setFilter] = createSignal<string>("");
  const [error, setError] = createSignal<unknown>(null);
  const [open, updateOpen] = createSignal<CalendarOccurrence | null>(null);
  const setOpen = (value: CalendarOccurrence | null) => { updateOpen(value); props.onSelectionChange?.(value); };
  const [editing, setEditing] = createSignal<{ event?: CalendarEvent; day?: number } | null>(null);
  createEffect(on(project, () => {
    updateOpen(null);
    setEditing(null);
    setError(null);
  }));

  const range = createMemo(() => {
    const a = anchor();
    if (mode() === "month") return monthRange(a);
    if (mode() === "week") {
      const from = weekStart(a);
      return { from, to: from + 8 * DAY };
    }
    const d = new Date(a);
    const from = new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
    return { from, to: from + 31 * DAY };
  });

  const [view, { refetch }] = createResource(
    () => ({ p: project(), r: range(), f: filter() }),
    ({ p, r, f }): Promise<CalendarView> => {
      const q = { from: new Date(r.from).toISOString(), to: new Date(r.to).toISOString() };
      if (p) return api.calendar.project(p, q);
      if (f === "global") return api.calendar.global(q);
      return api.calendar.combined(f ? { ...q, project: f } : q);
    },
  );
  const [projects] = createResource(
    () => project() === null,
    async (combined) => (combined ? (await api.projects.list()).map((p) => p.name) : []),
    { initialValue: [] },
  );
  const [watching, { refetch: refetchWatch }] = createResource(
    () => ({ p: project() }),
    async ({ p }) => (await api.calendar.watchers(p)).some((w) => w.watcher === "operator"),
  );

  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (!e.kind.startsWith("calendar.")) return;
      if (project() && e.project && e.project !== project()) return;
      clearTimeout(t);
      t = setTimeout(() => void refetch(), 400);
    });
    const onPhone = () => props.legacy && phone?.matches && mode() === "month" && setMode("agenda");
    phone?.addEventListener?.("change", onPhone);
    onCleanup(() => (off(), clearTimeout(t), phone?.removeEventListener?.("change", onPhone)));
  });

  const occ = () => view.latest?.occurrences ?? [];
  const [days, setDays] = createStore<{ month: ReturnType<typeof monthGrid>; week: ReturnType<typeof weekDays>; agenda: ReturnType<typeof agenda> }>({ month: [], week: [], agenda: [] });
  createEffect(() => {
    setDays("month", reconcile(monthGrid(anchor(), occ(), now()), { key: "key" }));
    setDays("week", reconcile(weekDays(anchor(), occ(), now()), { key: "key" }));
    setDays("agenda", reconcile(agenda(occ(), range().from), { key: "day" }));
  });
  const eventOf = (id: string) => view.latest?.events.find((e) => e.id === id);
  const canWrite = () => (project() ? !!view.latest?.can_write : true);

  const step = (n: number) => {
    const d = new Date(anchor());
    if (mode() === "month") setAnchor(new Date(d.getFullYear(), d.getMonth() + n, 1).getTime());
    else if (mode() === "week") setAnchor(d.getTime() + n * 7 * DAY);
    else setAnchor(d.getTime() + n * 31 * DAY);
  };
  const title = () => {
    const d = new Date(anchor());
    if (mode() === "month") return d.toLocaleDateString(undefined, { month: "long", year: "numeric" });
    const r = range();
    const last = new Date(r.to - (mode() === "week" ? 2 : 1) * DAY);
    return `${new Date(r.from).toLocaleDateString()} – ${last.toLocaleDateString()}`;
  };
  const scopes = () => [{ value: null, label: "Global (every project)" }, ...projects().map((p) => ({ value: p as string | null, label: p }))];

  const toggleWatch = async () => {
    setError(null);
    try {
      if (watching()) await api.calendar.unwatch(project(), "operator");
      else await api.calendar.watch(project(), "operator");
      await refetchWatch();
    } catch (e) {
      setError(e);
    }
  };

  const showScope = () => project() === null;
  const openEvent = () => {
    const o = open();
    return o ? eventOf(o.event_id) : undefined;
  };

  return (
    <>
      <PageHead title={project() ? "Calendar" : "Calendar: every project"} level={project() ? 2 : 1}>
        <button onClick={() => void toggleWatch()} title="A mail in the Inbox for each add, change, and removal">
          {watching() ? "Stop watching" : project() === null ? "Watch the global calendar" : "Watch in the Inbox"}
        </button>
        <Show when={canWrite()}>
          <button class="primary" onClick={() => setEditing({})}>
            New event
          </button>
        </Show>
      </PageHead>
      <p class="muted small">
        {project()
          ? "Maintenance windows, change freezes, deploy windows, and planned work of the project, with the global events. Agents add them with "
          : "The calendars of every project and the global calendar. Agents add events with "}
        <code>terrarium calendar add</code>; workers read only, unless a lead grants <code>terrarium calendar grant</code>.
      </p>
      <div class="cal-toolbar">
        <Tabs tabs={MODES} active={mode()} onChange={setMode} />
        <div class="row cal-nav">
          <button aria-label="Back" onClick={() => step(-1)}>
            ‹
          </button>
          <button onClick={() => setAnchor(Date.now())}>Today</button>
          <button aria-label="On" onClick={() => step(1)}>
            ›
          </button>
          <strong class="cal-title">{title()}</strong>
        </div>
        <Show when={project() === null}>
          <label class="row small">
            Show
            <select value={filter()} onChange={(e) => setFilter(e.currentTarget.value)}>
              <option value="">Every project</option>
              <option value="global">The global calendar</option>
              <For each={projects()}>{(p) => <option value={p}>{p}</option>}</For>
            </select>
          </label>
        </Show>
      </div>
      <ErrorBanner error={error() ?? view.error} onDismiss={() => setError(null)} />
      <Show when={view.latest?.truncated}>
        <p class="small mon-err">The list is cut; show a shorter window.</p>
      </Show>
      <Show when={mode() === "month"}>
        <div class="cal-month" role="grid" aria-label={title()}>
          <div class="cal-row cal-head" role="row">
            <For each={DAYS}>
              {(d) => (
                <div class="cal-dow" role="columnheader">
                  {DAY_LABELS[d]}
                </div>
              )}
            </For>
          </div>
          <For each={days.month}>
            {(week) => (
              <div class="cal-row" role="row">
                <For each={week}>
                  {(d) => (
                    <div class="cal-cell" role="gridcell" classList={{ "cal-out": !d.inMonth, "cal-today": d.today }}>
                      <div class="cal-cell-head">
                        <span class="cal-date">{new Date(d.day).getDate()}</span>
                        <Show when={canWrite()}>
                          <button class="link cal-add" aria-label={`New event on ${d.key}`} onClick={() => setEditing({ day: d.day })}>
                            +
                          </button>
                        </Show>
                      </div>
                      <For each={d.items.slice(0, 4)}>{(o) => <Chip o={o} now={now()} showScope={showScope()} onOpen={setOpen} />}</For>
                      <Show when={d.items.length > 4}>
                        <button
                          class="link small"
                          onClick={() => {
                            setAnchor(d.day);
                            setMode("week");
                          }}
                        >
                          {d.items.length - 4} more
                        </button>
                      </Show>
                    </div>
                  )}
                </For>
              </div>
            )}
          </For>
        </div>
      </Show>
      <Show when={mode() === "week"}>
        <div class="cal-week">
          <For each={days.week}>
            {(d) => (
              <section class="cal-week-day" classList={{ "cal-today": d.today }} aria-label={d.key}>
                <h3 class="small">
                  {new Date(d.day).toLocaleDateString(undefined, { weekday: "short", day: "numeric", month: "short" })}
                </h3>
                <For each={d.items} fallback={<span class="muted small">–</span>}>
                  {(o) => <Chip o={o} now={now()} showScope={showScope()} onOpen={setOpen} />}
                </For>
              </section>
            )}
          </For>
        </div>
      </Show>
      <Show when={mode() === "agenda"}>
        <Show when={occ().length} fallback={<Empty>{view.loading ? "Loading…" : "Nothing on the calendar in these 31 days."}</Empty>}>
          <div class="cal-agenda">
            <For each={days.agenda}>
              {(g) => (
                <section aria-label={g.key} classList={{ "cal-today": g.key === dayKey(now()) }}>
                  <h3 class="cal-agenda-day">{new Date(g.day).toLocaleDateString(undefined, { weekday: "long", day: "numeric", month: "long" })}</h3>
                  <ul class="cal-agenda-list">
                    <For each={g.items}>
                      {(o) => (
                        <li>
                          <button class={`cal-agenda-item ${kindClass(o.kind)}`} classList={{ "cal-live": isRunning(o, now()) }} onClick={() => setOpen(o)}>
                            <span class="cal-agenda-time">{timeLabel(o)}</span>
                            <span class="cal-agenda-title">{o.title}</span>
                            <span class="muted small">
                              {kindLabel(o.kind)}
                              {o.global || showScope() ? ` · ${scopeLabel(o)}` : ""}
                              {o.recurring ? " · repeats" : ""}
                              {o.assignees.length ? ` · ${o.assignees.join(", ")}` : ""}
                            </span>
                          </button>
                        </li>
                      )}
                    </For>
                  </ul>
                </section>
              )}
            </For>
          </div>
        </Show>
      </Show>
      <Show when={open() && openEvent()}>
        <EventDetail
          event={openEvent()!}
          occurrence={open()!}
          now={now()}
          onClose={() => setOpen(null)}
          onEdit={() => {
            setEditing({ event: openEvent()! });
            setOpen(null);
          }}
          onChanged={() => void refetch()}
        />
      </Show>
      <Show when={editing()}>
        {(ed) => (
          <EventDialog
            scope={project()}
            scopes={project() === null ? scopes() : undefined}
            event={ed().event}
            day={ed().day}
            now={now()}
            onClose={() => setEditing(null)}
            onSaved={() => {
              setEditing(null);
              void refetch();
            }}
          />
        )}
      </Show>
    </>
  );
}
