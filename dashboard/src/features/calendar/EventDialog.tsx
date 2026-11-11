import { useInstance } from "../../instance";
// The create and edit dialog of a calendar event (docs/api.md §33): the
// title, the kind, the times in the event's zone (or the days of an
// all-day event), the repeat rule, the assignees, the links, and the
// description in markdown.

import { createResource, createSignal, For, Show } from "solid-js";
import type { CalendarDay, CalendarEvent } from "../../api/types";
import { ErrorBanner, Modal } from "../../components/ui";

import { DAY_LABELS, emptyForm, formError, formOf, inputOf, KINDS, type EventForm } from "./model";
import { DAYS } from "./recur";

export function EventDialog(props: {
  /** The calendar of a new event: a project, or null for the global one. */
  scope: string | null;
  /** The calendars to pick from in the combined view (a new event only). */
  scopes?: { value: string | null; label: string }[];
  event?: CalendarEvent;
  /** The day that a month cell gave. */
  day?: number;
  now: number;
  onClose: () => void;
  onSaved: (e: CalendarEvent) => void;
}) {
  const { api } = useInstance();
  const [f, setF] = createSignal<EventForm>(props.event ? formOf(props.event) : emptyForm(props.now, props.day));
  const set = <K extends keyof EventForm>(k: K, v: EventForm[K]) => setF((x) => ({ ...x, [k]: v }));
  const [scope, setScope] = createSignal<string | null>(props.event ? props.event.project : props.scope);
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  // The assignees: the agents of the project; any agent for a global event. (An object source: a null scope fetches too.)
  const [agents] = createResource(
    () => ({ s: scope() }),
    async ({ s }) => (await api.agents.list(s ?? undefined)).sort((a, b) => `${a.project}/${a.name}`.localeCompare(`${b.project}/${b.name}`)),
    { initialValue: [] },
  );
  const pickable = () => agents();
  const problem = () => formError(f());
  const toggleDay = (d: CalendarDay) => set("weekdays", f().weekdays.includes(d) ? f().weekdays.filter((x) => x !== d) : [...f().weekdays, d]);
  const toggleAgent = (id: string) => set("assignees", f().assignees.includes(id) ? f().assignees.filter((x) => x !== id) : [...f().assignees, id]);

  const save = (e: Event) => {
    e.preventDefault();
    if (problem()) return;
    setBusy(true);
    setError(null);
    void (async () => {
      try {
        const saved = props.event ? await api.calendar.update(props.event.id, inputOf(f(), true)) : await api.calendar.create(scope(), inputOf(f()));
        props.onSaved(saved);
      } catch (x) {
        setError(x);
      } finally {
        setBusy(false);
      }
    })();
  };

  return (
    <Modal title={props.event ? `Edit ${props.event.title}` : "New calendar event"} onClose={() => props.onClose()} class="modal-wide modal-full">
      <form class="stack mon-form" onSubmit={save}>
        <div class="mon-form-grid">
          <label class="span-2">
            Title
            <input required maxLength={200} value={f().title} onInput={(e) => set("title", e.currentTarget.value)} placeholder="Weekly patch window" />
          </label>
          <label>
            Kind
            <select value={f().kind} onChange={(e) => set("kind", e.currentTarget.value as EventForm["kind"])}>
              <For each={KINDS}>{(k) => <option value={k.kind}>{k.label}</option>}</For>
            </select>
          </label>
          <Show
            when={!props.event && props.scopes?.length}
            fallback={
              <label>
                Calendar
                <input disabled value={scope() ?? "global"} />
              </label>
            }
          >
            <label>
              Calendar
              <select value={scope() ?? ""} onChange={(e) => (setScope(e.currentTarget.value || null), set("assignees", []))}>
                <For each={props.scopes}>{(s) => <option value={s.value ?? ""}>{s.label}</option>}</For>
              </select>
            </label>
          </Show>
        </div>
        <label class="row small">
          <input type="checkbox" checked={f().allDay} onChange={(e) => set("allDay", e.currentTarget.checked)} /> All day
        </label>
        <div class="mon-form-grid cal-times">
          <label>
            {f().allDay ? "First day" : "Start"}
            <input type="date" required value={f().startDate} onInput={(e) => set("startDate", e.currentTarget.value)} />
          </label>
          <Show when={!f().allDay}>
            <label>
              Start time
              <input type="time" required value={f().startTime} onInput={(e) => set("startTime", e.currentTarget.value)} />
            </label>
          </Show>
          <label>
            {f().allDay ? "Last day" : "End"}
            <input type="date" required value={f().endDate} onInput={(e) => set("endDate", e.currentTarget.value)} />
          </label>
          <Show when={!f().allDay}>
            <label>
              End time
              <input type="time" required value={f().endTime} onInput={(e) => set("endTime", e.currentTarget.value)} />
            </label>
          </Show>
          <label class="span-2">
            Time zone <span class="muted small">(IANA, the times above and the repeat)</span>
            <input value={f().tz} onInput={(e) => set("tz", e.currentTarget.value.trim())} placeholder="Europe/London" />
          </label>
        </div>
        <fieldset class="cal-repeat">
          <legend>Repeat</legend>
          <div class="mon-form-grid">
            <label>
              Repeat
              <select value={f().repeat} onChange={(e) => set("repeat", e.currentTarget.value as EventForm["repeat"])}>
                <option value="none">Does not repeat</option>
                <option value="daily">Daily</option>
                <option value="weekly">Weekly</option>
                <option value="monthly">Monthly</option>
              </select>
            </label>
            <Show when={f().repeat !== "none"}>
              <label>
                Every
                <input type="number" min="1" max="999" value={f().interval} onInput={(e) => set("interval", e.currentTarget.value)} />
              </label>
            </Show>
          </div>
          <Show when={f().repeat === "weekly"}>
            <div class="row cal-days" role="group" aria-label="Weekdays">
              <For each={DAYS}>
                {(d) => (
                  <label class="row small">
                    <input type="checkbox" checked={f().weekdays.includes(d)} onChange={() => toggleDay(d)} /> {DAY_LABELS[d]}
                  </label>
                )}
              </For>
              <span class="muted small">None: the weekday of the start.</span>
            </div>
          </Show>
          <Show when={f().repeat === "monthly"}>
            <label>
              Day of the month <span class="muted small">(empty: the day of the start; -1: the last day)</span>
              <input type="number" min="-1" max="31" value={f().monthDay} onInput={(e) => set("monthDay", e.currentTarget.value)} />
            </label>
          </Show>
          <Show when={f().repeat !== "none"}>
            <div class="mon-form-grid">
              <label>
                Ends
                <select value={f().endMode} onChange={(e) => set("endMode", e.currentTarget.value as EventForm["endMode"])}>
                  <option value="never">Never</option>
                  <option value="until">On a date</option>
                  <option value="count">After a number of times</option>
                </select>
              </label>
              <Show when={f().endMode === "until"}>
                <label>
                  Last date
                  <input type="date" value={f().until} onInput={(e) => set("until", e.currentTarget.value)} />
                </label>
              </Show>
              <Show when={f().endMode === "count"}>
                <label>
                  Times
                  <input type="number" min="1" max="10000" value={f().count} onInput={(e) => set("count", e.currentTarget.value)} />
                </label>
              </Show>
            </div>
          </Show>
        </fieldset>
        <fieldset class="cal-assignees">
          <legend>Assignees</legend>
          <p class="muted small" style={{ margin: 0 }}>
            Each assignee gets a mail when an occurrence starts and when it ends.
          </p>
          <div class="cal-agent-list">
            <For each={pickable()} fallback={<span class="muted small">No agents.</span>}>
              {(a) => (
                <label class="row small">
                  <input type="checkbox" checked={f().assignees.includes(a.id)} onChange={() => toggleAgent(a.id)} />
                  {scope() === null ? `${a.project}/${a.name}` : a.name}
                </label>
              )}
            </For>
          </div>
        </fieldset>
        <div class="mon-form-grid">
          <label>
            Card <span class="muted small">(optional)</span>
            <input value={f().card} onInput={(e) => set("card", e.currentTarget.value)} placeholder="TER-16" />
          </label>
          <label>
            Target <span class="muted small">(optional)</span>
            <input value={f().target} onInput={(e) => set("target", e.currentTarget.value)} placeholder="build-host" />
          </label>
          <label>
            Repo <span class="muted small">(optional)</span>
            <input value={f().repo} onInput={(e) => set("repo", e.currentTarget.value)} />
          </label>
          <label>
            URL <span class="muted small">(optional)</span>
            <input type="url" value={f().url} onInput={(e) => set("url", e.currentTarget.value)} placeholder="https://" />
          </label>
        </div>
        <label>
          Description <span class="muted small">(markdown)</span>
          <textarea rows={5} value={f().description} onInput={(e) => set("description", e.currentTarget.value)} />
        </label>
        <Show when={problem()}>{(p) => <div class="small mon-err">{p()}</div>}</Show>
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <div class="row" style={{ "justify-content": "flex-end", "flex-wrap": "wrap" }}>
          <button type="button" class="ghost" onClick={() => props.onClose()}>
            Cancel
          </button>
          <button type="submit" class="primary" disabled={busy() || !!problem()}>
            {busy() ? "Saving…" : props.event ? "Save" : "Add event"}
          </button>
        </div>
      </form>
    </Modal>
  );
}
