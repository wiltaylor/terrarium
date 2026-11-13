import { useInstance } from "../../instance";
// The event sources of the Triggers tab (docs/api.md §20): the list, create,
// enable and disable, rotate, and delete. A create and a rotate answer with
// the secret: it shows one time in a panel, with the hook URL and the setup
// steps of the sender, and the panel drops it when it closes. `event_source`
// events, and the events that a source adds to the log, read the list again.

import { createEffect, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { ConfirmButton, CopyButton, Empty, ErrorBanner, Modal, useNow } from "../../components/ui";
import { ApiError } from "../../api/http";
import type { EventSource, EventSourceKind, EventSourceWithSecret } from "../../api/types";

import { ago } from "../../lib/format";
import { GLOBAL } from "./form";
import { emptySourceForm, sortSources, SOURCE_KINDS, sourceBody, sourceFieldOf, sourceScopeLabel, upsertSource, type SourceForm, type SourceFormKey } from "./sources";

const COLS = 8;

type EventSourcesProps = { project: string | null; search?: string; canCreate?: boolean; canManage?: (source: EventSource) => boolean };

export function EventSourcesSection(props: EventSourcesProps) {
  return <Show when={{ project: props.project }} keyed>{(scope) => <EventSourcesBody {...props} project={scope.project} />}</Show>;
}

function EventSourcesBody(props: EventSourcesProps) {
  const { api, events } = useInstance();
  const [data, setData] = createStore<{ items: EventSource[] }>({ items: [] });
  const items = () => data.items;
  const visible = () => items().filter((s) => !props.search || s.name.toLowerCase().includes(props.search.toLowerCase()));
  const setItems = (next: EventSource[] | ((previous: EventSource[]) => EventSource[])) => setData("items", reconcile(typeof next === "function" ? next(items()) : next));
  const [loading, setLoading] = createSignal(true);
  const [error, setError] = createSignal<unknown>(null);
  const [rowError, setRowError] = createSignal<{ id: string; error: unknown } | null>(null);
  const [busy, setBusy] = createSignal<string | null>(null);
  const [creating, setCreating] = createSignal(false);
  const [setupOf, setSetupOf] = createSignal<string | null>(null);
  // The answer of a create or a rotate, with its secret. Only the panel reads it; closing the panel drops it.
  const [revealed, setRevealed] = createSignal<{ source: EventSourceWithSecret; rotated: boolean } | null>(null);
  const now = useNow(15_000);
  let generation = 0;
  onCleanup(() => { generation++; });

  const load = async () => {
    const gen = ++generation;
    try {
      const list = await api.eventSources.list(props.project ? { project: props.project } : {});
      if (gen !== generation) return;
      setItems(sortSources(list));
      setError(null);
    } catch (e) {
      if (gen === generation) setError(e);
    } finally {
      if (gen === generation) setLoading(false);
    }
  };

  createEffect(
    on(
      () => props.project,
      () => {
        setLoading(true);
        setItems([]);
        setCreating(false);
        setSetupOf(null);
        setRevealed(null);
        setRowError(null);
        setBusy(null);
        void load();
      },
    ),
  );

  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      // A change of a source, or an event that a source added (its `received` and `last_seen` moved).
      if (e.kind !== "event_source" && !e.actor?.startsWith("source:")) return;
      clearTimeout(t);
      t = setTimeout(() => void load(), e.kind === "event_source" ? 300 : 1500);
    });
    onCleanup(() => {
      off();
      clearTimeout(t);
    });
  });

  const guard = async (s: EventSource, what: string, fn: () => Promise<void>) => {
    setBusy(`${s.id}:${what}`);
    try {
      await fn();
      setRowError(null);
    } catch (e) {
      setRowError({ id: s.id, error: e });
    } finally {
      setBusy(null);
    }
  };
  const toggle = (s: EventSource, el: HTMLInputElement) =>
    guard(s, "toggle", async () => {
      try {
        const next = await api.eventSources.update(s.id, { enabled: el.checked });
        setItems((l) => upsertSource(l, next));
      } catch (e) {
        el.checked = s.enabled;
        throw e;
      }
    });
  const rotate = (s: EventSource) =>
    guard(s, "rotate", async () => {
      const answer = await api.eventSources.rotate(s.id);
      setItems((l) => upsertSource(l, answer));
      setRevealed({ source: answer, rotated: true });
    });
  const remove = (s: EventSource) =>
    guard(s, "delete", async () => {
      await api.eventSources.remove(s.id);
      setItems((l) => l.filter((x) => x.id !== s.id));
    });

  return (
    <section class="card-box" style={{ "margin-bottom": "16px" }} aria-label="Event sources">
      <div class="spread" style={{ "flex-wrap": "wrap", gap: "8px", "margin-bottom": "8px" }}>
        <div class="stack" style={{ gap: "2px" }}>
          <h3 style={{ margin: 0 }}>Event sources</h3>
          <span class="small muted">Webhook senders (GitHub, Forgejo, or a script). Each good request becomes one event in the log; a trigger acts on it.</span>
        </div>
        <span class="row">
          <Show when={props.canCreate !== false}><button class="primary small" onClick={() => setCreating(true)}>
            New event source
          </button></Show>
          <button class="small" onClick={() => void load()}>
            Refresh
          </button>
        </span>
      </div>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <div class="table-wrap">
        <table aria-label="Event sources">
          <thead>
            <tr>
              <th>Name</th>
              <th>Scope</th>
              <th>Kind</th>
              <th>Received</th>
              <th>Last seen</th>
              <th>Last error</th>
              <th>Enabled</th>
              <th />
            </tr>
          </thead>
          <tbody>
            <Show when={visible().length} fallback={<tr><td colSpan={COLS}><Empty>{loading() ? "Loading…" : props.search && items().length ? "No event sources match the search." : "No event sources. Make one with New event source, then add its hook URL and secret to the forge."}</Empty></td></tr>}>
              <For each={visible()}>
                {(s) => (
                  <>
                    <tr aria-label={`Event source ${s.name}`}>
                      <td>
                        <strong>{s.name}</strong>
                        <div class="small muted mono ellipsis" style={{ "max-width": "16rem" }} title={s.hook_url ?? s.hook_path}>
                          {s.hook_path}
                        </div>
                      </td>
                      <td>
                        <span class={`chip ${s.project ? "" : "info"}`} title={s.project ? `A source of project ${s.project}` : "A global source: its events have no project"}>
                          {sourceScopeLabel(s)}
                        </span>
                      </td>
                      <td>{s.kind}</td>
                      <td>{s.received}</td>
                      <td class="small">
                        <Show when={s.last_seen} fallback={<span class="muted">never</span>}>
                          {(at) => <span title={new Date(at()).toLocaleString()}>{ago(at(), now())}</span>}
                        </Show>
                      </td>
                      <td class="small">
                        <Show when={s.last_error} fallback={<span class="muted">—</span>}>
                          {(err) => (
                            <span class="chip warn ellipsis" style={{ "max-width": "16rem" }} title={err()}>
                              {err()}
                            </span>
                          )}
                        </Show>
                      </td>
                      <td>
                        <Show when={!props.canManage || props.canManage(s)} fallback={<span>{s.enabled ? "On" : "Off"}</span>}><input type="checkbox" aria-label={`Enable ${s.name}`} checked={s.enabled} disabled={busy() === `${s.id}:toggle`} onChange={(e) => void toggle(s, e.currentTarget)} /></Show>
                      </td>
                      <td>
                        <span class="row" style={{ "flex-wrap": "wrap" }}>
                          <button class="small" aria-expanded={setupOf() === s.id} onClick={() => setSetupOf((cur) => (cur === s.id ? null : s.id))}>
                            {setupOf() === s.id ? "Hide setup" : "Setup"}
                          </button>
                          <Show when={!props.canManage || props.canManage(s)}><ConfirmButton class="small" label="Rotate" confirmLabel="Yes, rotate: the old secret stops now" disabled={busy() === `${s.id}:rotate`} onConfirm={() => rotate(s)} />
                          <ConfirmButton class="small danger" label="Delete" disabled={busy() === `${s.id}:delete`} onConfirm={() => remove(s)} /></Show>
                        </span>
                      </td>
                    </tr>
                    <Show when={rowError()?.id === s.id ? rowError()!.error : null}>
                      {(err) => (
                        <tr class="cap-row">
                          <td colSpan={COLS}>
                            <ErrorBanner error={err()} onDismiss={() => setRowError(null)} />
                          </td>
                        </tr>
                      )}
                    </Show>
                    <Show when={setupOf() === s.id}>
                      <tr class="lease-row">
                        <td colSpan={COLS}>
                          <SetupSteps source={s} />
                          <p class="small muted" style={{ margin: "6px 0 0" }}>
                            The secret shows only at create and at rotate. Lost it? Rotate the source, then put the new secret in the forge.
                          </p>
                        </td>
                      </tr>
                    </Show>
                  </>
                )}
              </For>
            </Show>
          </tbody>
        </table>
      </div>
      <Show when={creating()}>
        <NewSourceDialog
          project={props.project}
          onClose={() => setCreating(false)}
          onCreated={(answer) => {
            setCreating(false);
            setItems((l) => upsertSource(l, answer));
            setRevealed({ source: answer, rotated: false });
          }}
        />
      </Show>
      <Show when={revealed()}>{(r) => <SecretPanel source={r().source} rotated={r().rotated} onClose={() => setRevealed(null)} />}</Show>
    </section>
  );
}

/** The hook URL and the setup steps of the sender. */
function SetupSteps(props: { source: EventSource }) {
  return (
    <div class="stack" style={{ gap: "4px" }} aria-label={`Setup of ${props.source.name}`}>
      <div class="copy-row">
        <span class="small">Hook URL</span>
        <code class="mono" aria-label="Hook URL">
          {props.source.hook_url ?? props.source.hook_path}
        </code>
        <CopyButton text={props.source.hook_url ?? props.source.hook_path} label="Copy URL" />
      </div>
      <Show when={props.source.hook_note}>{(n) => <span class="small muted">{n()}</span>}</Show>
      <Show when={props.source.hook_url === null}>
        <span class="small chip warn">The ingest listener is off: set TERRARIUM_INGEST_ADDR (and TERRARIUM_INGEST_PUBLIC_URL), then restart the daemon.</span>
      </Show>
      <ol class="small" style={{ margin: "4px 0", "padding-left": "1.2rem" }}>
        <For each={props.source.setup}>{(step) => <li>{step}</li>}</For>
      </ol>
      <span class="small muted">
        The content type must be <code>application/json</code>: a form body gets <code>415</code>.
      </span>
    </div>
  );
}

/** The secret of a create or a rotate, one time. Closing the panel drops it for good. */
function SecretPanel(props: { source: EventSourceWithSecret; rotated: boolean; onClose: () => void }) {
  return (
    <Modal title={`${props.rotated ? "New secret" : "Secret"} of ${props.source.name}`} onClose={() => props.onClose()}>
      <div class="stack">
        <p class="notice" style={{ margin: 0 }} role="status">
          Copy the secret now. <strong>You will not see it again</strong>: the daemon shows it only in this answer.
          {props.rotated ? " The old secret stopped at once; put this one in the forge." : ""}
        </p>
        <div class="copy-row">
          <code class="token-once" aria-label="Secret">
            {props.source.secret}
          </code>
          <CopyButton text={props.source.secret} label="Copy secret" />
        </div>
        <SetupSteps source={props.source} />
        <div>
          <button class="primary" onClick={() => props.onClose()}>
            Done, I copied it
          </button>
        </div>
      </div>
    </Modal>
  );
}

function NewSourceDialog(props: { project: string | null; onClose: () => void; onCreated: (s: EventSourceWithSecret) => void }) {
  const { api, projects } = useInstance();
  const [form, setForm] = createSignal<SourceForm>(emptySourceForm(props.project));
  const [sending, setSending] = createSignal(false);
  const [bad, setBad] = createSignal<{ field?: SourceFormKey; message: string } | null>(null);
  const [error, setError] = createSignal<unknown>(null);
  const set = <K extends SourceFormKey>(k: K, v: SourceForm[K]) => {
    setForm((f) => ({ ...f, [k]: v }));
    setBad(null);
  };
  const fieldError = (k: SourceFormKey) => (bad()?.field === k ? bad()!.message : null);

  const submit = async (e: Event) => {
    e.preventDefault();
    if (sending()) return;
    const b = sourceBody(form());
    if ("error" in b) return setBad({ field: b.field, message: b.error });
    setSending(true);
    try {
      props.onCreated(await api.eventSources.create(b.body));
    } catch (err) {
      if (err instanceof ApiError && (err.status === 422 || err.status === 409 || err.status === 403)) {
        const d = err.details as { field?: unknown } | undefined;
        setBad({ field: err.status === 409 ? "name" : err.status === 403 ? "scope" : sourceFieldOf(d?.field), message: err.message });
      } else setError(err);
    } finally {
      setSending(false);
    }
  };

  const errorUnder = (k: SourceFormKey, label: string) => (
    <Show when={fieldError(k)}>
      {(m) => (
        <span class="field-error small" role="alert" aria-label={`Error in ${label}`}>
          {m()}
        </span>
      )}
    </Show>
  );

  return (
    <Modal title="New event source" onClose={() => props.onClose()}>
      <form class="stack" onSubmit={submit} aria-label="New event source">
        <div class="form-grid">
          <div class="field" classList={{ "field-bad": !!fieldError("name") }}>
            <label>
              Name
              <input value={form().name} placeholder="gh" autocomplete="off" onInput={(e) => set("name", e.currentTarget.value)} />
            </label>
            {errorUnder("name", "Name")}
          </div>
          <div class="field" classList={{ "field-bad": !!fieldError("scope") }}>
            <label>
              Scope
              <select aria-label="Scope" value={form().scope} onChange={(e) => set("scope", e.currentTarget.value)}>
                <option value="" disabled selected={!form().scope}>
                  Pick a project, or global
                </option>
                <For each={projects.projects()}>
                  {(p) => (
                    <option value={p.name} selected={p.name === form().scope}>
                      Project {p.name}
                    </option>
                  )}
                </For>
                <option value={GLOBAL} selected={form().scope === GLOBAL}>
                  Global: events with no project (needs sysadmin)
                </option>
              </select>
            </label>
            {errorUnder("scope", "Scope")}
          </div>
          <div class="field" classList={{ "field-bad": !!fieldError("kind") }}>
            <label>
              Kind
              <select aria-label="Kind" value={form().kind} onChange={(e) => set("kind", e.currentTarget.value as EventSourceKind)}>
                <For each={SOURCE_KINDS}>
                  {(k) => (
                    <option value={k.id} selected={k.id === form().kind}>
                      {k.label}
                    </option>
                  )}
                </For>
              </select>
            </label>
            {errorUnder("kind", "Kind")}
          </div>
        </div>
        <span class="small muted">The daemon makes a random secret and shows it one time, in the next step.</span>
        <Show when={bad() && !bad()!.field}>{(_) => <p class="error-banner small" role="alert">{bad()!.message}</p>}</Show>
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <div class="row" style={{ "justify-content": "flex-end" }}>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
          <button class="primary" type="submit" disabled={sending()}>
            {sending() ? "Creating…" : "Create"}
          </button>
        </div>
      </form>
    </Modal>
  );
}
