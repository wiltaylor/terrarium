import { useInstance } from "../../instance";
// The Waits and Watches panels of the agent page. A wait holds the agent in
// `waiting`; a watch sends mail when the watched agent changes status.
// Process output is untrusted: it goes into the page as text nodes only.

import { A } from "@solidjs/router";
import { createEffect, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import { WATCH_ON, type Agent, type Wait, type WaitLog, type Watch, type WatchOn } from "../../api/types";
import { ConfirmButton, Empty, ErrorBanner, useNow } from "../../components/ui";
import { ago } from "../../lib/format";
import { actorLabel, countdown } from "../../lib/leases";

import { isWaitEventFor, putWait, splitWaits, toggleOn, waitCommand, waitResultText, waitStateChip, watchBody, watchRole, watchText } from "./waits";

const agentName = (id: string) => {
  const { agents } = useInstance();
  const a = agents.byId(id);
  return a ? `${a.project}/${a.name}` : undefined;
};

/** Read a list again soon after an event. Many events in a burst give one request. */
function useSoon(fn: () => void, ms = 300) {
  let t: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(t));
  return () => {
    clearTimeout(t);
    t = setTimeout(fn, ms);
  };
}

/** The output tail of a process wait (`GET /api/waits/{id}/log`), as plain text. */
function WaitLogView(props: { wait: Wait; onClose: () => void }) {
  const { api } = useInstance();
  const [log, setLog] = createSignal<WaitLog | null>(null);
  const [error, setError] = createSignal<unknown>(null);
  const [loading, setLoading] = createSignal(false);
  const load = async () => {
    setLoading(true);
    try {
      setLog(await api.waits.log(props.wait.id));
      setError(null);
    } catch (e) {
      setError(e);
    } finally {
      setLoading(false);
    }
  };
  onMount(() => void load());
  return (
    <div class="stack" style={{ gap: "4px" }} aria-label={`Output of ${props.wait.summary}`}>
      <ErrorBanner error={error()} />
      <div class="row small" style={{ "flex-wrap": "wrap" }}>
        <Show when={log()}>
          {(l) => (
            <>
              <span class={`chip ${l().running ? "info" : ""}`}>{l().running ? "running" : "ended"}</span>
              <span class="muted mono">
                {l().target} · {l().session_id}
              </span>
            </>
          )}
        </Show>
        <button class="small" disabled={loading()} onClick={() => void load()}>
          {loading() ? "Loading…" : "Refresh"}
        </button>
        <button class="small" onClick={() => props.onClose()}>
          Close
        </button>
      </div>
      <Show when={log()}>
        {(l) => <pre class="run-output mono small">{l().output || "(no output)"}</pre>}
      </Show>
    </div>
  );
}

/** One wait: a row, and its log below when open. */
function WaitRow(props: { wait: Wait; now: number; closed?: boolean; onChange: (w: Wait) => void; onError: (e: unknown) => void }) {
  const { api } = useInstance();
  const [logOpen, setLogOpen] = createSignal(false);
  const w = () => props.wait;
  const cancel = async (kill: boolean) => {
    try {
      props.onChange(await api.waits.cancel(w().id, { kill }));
    } catch (e) {
      props.onError(e);
    }
  };
  return (
    <>
      <tr class={logOpen() ? "row-open" : undefined}>
        <td>
          <span class="chip">{w().kind}</span>
        </td>
        <td>
          <div>{w().summary}</div>
          <Show when={waitCommand(w())}>{(c) => <div class="mono small muted">{c()}</div>}</Show>
          <Show when={w().reason}>{(r) => <div class="small muted">{r()}</div>}</Show>
        </td>
        <td class="small" title={w().created_at}>
          {ago(w().created_at, props.now)}
          <div class="muted">by {actorLabel(w().created_by, agentName)}</div>
        </td>
        <Show
          when={props.closed}
          fallback={
            <td class="small">
              <Show when={w().timeout_at} fallback={<span class="muted">none</span>}>
                {(t) => <span title={t()}>{countdown(t(), props.now)}</span>}
              </Show>
            </td>
          }
        >
          <td class="small">
            <span class={`chip ${waitStateChip(w().state)}`}>{w().state.replace("_", " ")}</span>
            <div class="mono">{waitResultText(w())}</div>
            <Show when={w().resolved_at}>{(t) => <div class="muted" title={t()}>{ago(t(), props.now)}</div>}</Show>
          </td>
        </Show>
        <td>
          <span class="row" style={{ "flex-wrap": "wrap", gap: "4px" }}>
            <Show when={w().kind === "process"}>
              <button class="small" onClick={() => setLogOpen(!logOpen())}>
                {logOpen() ? "Hide log" : "Log"}
              </button>
            </Show>
            <Show when={w().state === "open"}>
              <ConfirmButton class="small" label="Cancel" confirmLabel="Yes, cancel the wait" onConfirm={() => cancel(false)} />
              <Show when={w().kind === "process"}>
                <ConfirmButton class="small danger" label="Kill" confirmLabel="Yes, kill the process" onConfirm={() => cancel(true)} />
              </Show>
            </Show>
          </span>
        </td>
      </tr>
      <Show when={logOpen()}>
        <tr>
          <td colSpan={5}>
            <WaitLogView wait={w()} onClose={() => setLogOpen(false)} />
          </td>
        </tr>
      </Show>
    </>
  );
}

function WaitTable(props: { waits: Wait[]; closed?: boolean; now: number; onChange: (w: Wait) => void; onError: (e: unknown) => void }) {
  return (
    <div class="table-wrap">
      <table>
        <thead>
          <tr>
            <th>Kind</th>
            <th>Waits on</th>
            <th>Created</th>
            <th>{props.closed ? "Result" : "Timeout"}</th>
            <th />
          </tr>
        </thead>
        <tbody>
          <For each={props.waits}>{(w) => <WaitRow wait={w} now={props.now} closed={props.closed} onChange={props.onChange} onError={props.onError} />}</For>
        </tbody>
      </table>
    </div>
  );
}

/** The agent's waits (`GET /api/agents/{id}/waits`), kept live from `wait` events. */
export function WaitsPanel(props: { agent: Agent }) {
  const { api, events } = useInstance();
  const now = useNow(1000);
  const [all, setAll] = createSignal(false);
  const [list, setList] = createSignal<Wait[]>([]);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  let gen = 0;
  const load = async () => {
    const mine = ++gen;
    setLoading(true);
    try {
      const rows = await api.agents.waits(props.agent.id, { all: all() });
      if (mine !== gen) return;
      setList(rows);
      setError(null);
    } catch (e) {
      if (mine === gen) setError(e);
    } finally {
      if (mine === gen) setLoading(false);
    }
  };
  createEffect(on([() => props.agent.id, all], () => void load()));
  const soon = useSoon(() => void load());
  onMount(() => onCleanup(events.on((e) => isWaitEventFor(e, props.agent.id) && soon())));
  // The open waits also show in the status; read again when the count changes.
  createEffect(on(() => props.agent.waits ?? 0, soon, { defer: true }));

  const parts = () => splitWaits(list());
  const change = (w: Wait) => {
    setList((xs) => (w.state !== "open" && !all() ? xs.filter((x) => x.id !== w.id) : putWait(xs, w)));
    setError(null);
  };

  return (
    <section class="card-box stack" style={{ gap: "8px" }} aria-label="Waits">
      <div class="spread">
        <h3 style={{ margin: 0 }}>Waits</h3>
        <label class="row small">
          <input type="checkbox" checked={all()} onChange={(e) => setAll(e.currentTarget.checked)} />
          closed
        </label>
      </div>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={parts().open.length} fallback={<Empty>{loading() ? "Loading…" : "No open waits."}</Empty>}>
        <WaitTable waits={parts().open} now={now()} onChange={change} onError={setError} />
      </Show>
      <Show when={all()}>
        <h4 style={{ margin: 0 }}>Closed</h4>
        <Show when={parts().closed.length} fallback={<Empty>{loading() ? "Loading…" : "No closed waits."}</Empty>}>
          <WaitTable waits={parts().closed} closed now={now()} onChange={change} onError={setError} />
        </Show>
      </Show>
    </section>
  );
}

/** The operator's form: "Notify me when <agent> is ...". */
function WatchForm(props: { agent: Agent; onAdded: (w: Watch) => void }) {
  const { api } = useInstance();
  const [on_, setOn] = createSignal<WatchOn[]>(["idle", "blocked", "error"]);
  const [once, setOnce] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const submit = async (e: Event) => {
    e.preventDefault();
    const b = watchBody(on_(), once());
    if ("error" in b) return setError(b.error);
    setBusy(true);
    try {
      props.onAdded(await api.agents.watch(props.agent.id, b.body));
      setError(null);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };
  return (
    <form class="stack" style={{ gap: "6px" }} onSubmit={submit} aria-label="Add a watch">
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <div class="row" style={{ "flex-wrap": "wrap" }}>
        <span class="small">Notify me when {props.agent.name} is</span>
        <For each={WATCH_ON}>
          {(s) => (
            <label class="row small">
              <input type="checkbox" checked={on_().includes(s)} onChange={(e) => setOn(toggleOn(on_(), s, e.currentTarget.checked))} />
              {s}
            </label>
          )}
        </For>
        <label class="row small">
          <input type="checkbox" checked={once()} onChange={(e) => setOnce(e.currentTarget.checked)} />
          once
        </label>
        <button class="small primary" type="submit" disabled={busy() || !on_().length}>
          Add watch
        </button>
      </div>
    </form>
  );
}

/** Watches on the agent and by it (`GET /api/agents/{id}/watches`), kept live from `watch` events. */
export function WatchesPanel(props: { agent: Agent }) {
  const { api, events } = useInstance();
  const now = useNow(30_000);
  const [list, setList] = createSignal<Watch[]>([]);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  let gen = 0;
  const load = async () => {
    const mine = ++gen;
    setLoading(true);
    try {
      const rows = await api.agents.watches(props.agent.id);
      if (mine !== gen) return;
      setList(rows);
      setError(null);
    } catch (e) {
      if (mine === gen) setError(e);
    } finally {
      if (mine === gen) setLoading(false);
    }
  };
  createEffect(on(() => props.agent.id, () => void load()));
  const soon = useSoon(() => void load());
  onMount(() => onCleanup(events.on((e) => e.kind === "watch" && soon())));

  const remove = async (w: Watch) => {
    try {
      await api.watches.remove(w.id);
      setList((xs) => xs.filter((x) => x.id !== w.id));
      setError(null);
    } catch (e) {
      setError(e);
    }
  };

  return (
    <section class="card-box stack" style={{ gap: "8px" }} aria-label="Watches">
      <h3 style={{ margin: 0 }}>Watches</h3>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={list().length} fallback={<Empty>{loading() ? "Loading…" : "No watches."}</Empty>}>
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Watcher</th>
                <th>Watches</th>
                <th>On</th>
                <th>Created</th>
                <th />
              </tr>
            </thead>
            <tbody>
              <For each={list()}>
                {(w) => (
                  <tr>
                    <td>
                      <Show when={w.watcher !== "operator"} fallback={<span>operator</span>}>
                        <A href={`/agents/${encodeURIComponent(w.watcher)}`}>{w.watcher_name}</A>
                      </Show>
                    </td>
                    <td>
                      <Show when={watchRole(w, props.agent.id) === "by"} fallback={<span class="muted">this agent</span>}>
                        <A href={`/agents/${encodeURIComponent(w.agent)}`}>{w.agent_name}</A>
                      </Show>
                    </td>
                    <td class="small">
                      {watchText(w)}
                      <Show when={w.last_fired_at}>{(t) => <div class="muted" title={t()}>last {ago(t(), now())}</div>}</Show>
                    </td>
                    <td class="small" title={w.created_at}>
                      {ago(w.created_at, now())}
                    </td>
                    <td>
                      <ConfirmButton class="small danger" label="Remove" onConfirm={() => remove(w)} />
                    </td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </Show>
      <WatchForm agent={props.agent} onAdded={(w) => setList((xs) => (xs.some((x) => x.id === w.id) ? xs : [...xs, w]))} />
    </section>
  );
}
