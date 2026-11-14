// The Services page (TER-1269, Services C7, docs/api.md §17.12): each
// service with its definition and its queue, the requests in each state,
// and cancel of an open request. The daemon applies the rights of the
// CLI: an admin of a service sees all its requests, any other caller only
// its own, and cancel takes the same check (`may_touch` in
// api/services.rs). The page shows what the daemon answers and adds no
// rights of its own.
//
// It reloads every 15 seconds. The lists are stores that a reload
// reconciles by service name and by request ID (rev-1269 finding 1), so a
// reload keeps each card and row mounted: an open details row, the state
// filter, an open cancel confirm, and an error banner stay.

import { useSearchParams } from "@solidjs/router";
import { createEffect, createSignal, For, on, onCleanup, Show } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import type { Service, ServiceDetail, ServiceRequest, ServiceRequestState } from "../../api/types";
import { ConfirmButton, Empty, ErrorBanner, PageHead } from "../../components/ui";
import { useInstance } from "../../instance";
import { ago, bytes } from "../../lib/format";

import { callersText, claimText, countsNote, filterServices, isOpen, jsonText, outcomeText, REQUEST_STATES, runsText, scopeOptions, stateCounts, stateLabel, stateTone } from "./model";

/** How often the page reloads the services and their queues. */
export const REFRESH_MS = 15_000;
/** The most requests that one card loads (newest first). */
export const REQUEST_LIMIT = 100;

export default function ServicesPage() {
  const { api } = useInstance();
  const [search, setSearch] = useSearchParams<{ project?: string }>();
  const [tick, setTick] = createSignal(0);
  const [services, setServices] = createStore<Service[]>([]);
  const [loaded, setLoaded] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const load = async () => {
    try {
      setServices(reconcile(await api.services.list(), { key: "name" }));
      setError(null);
    } catch (e) {
      setError(e);
    } finally {
      setLoaded(true);
    }
  };
  void load();
  const reload = () => {
    void load();
    setTick((n) => n + 1);
  };
  const timer = setInterval(reload, REFRESH_MS);
  onCleanup(() => clearInterval(timer));
  const scope = () => search.project ?? "";
  const shown = () => filterServices([...services], scope());
  return (
    <div class="stack">
      <PageHead title="Services">
        <Show when={services.length > 1}>
          <select aria-label="Project" value={scope()} onChange={(e) => setSearch({ project: e.currentTarget.value || undefined })}>
            <option value="">All services</option>
            <For each={scopeOptions([...services])}>{(k) => <option value={k} selected={k === scope()}>{k === "global" ? "Global" : `Project ${k}`}</option>}</For>
          </select>
        </Show>
        <button class="small" type="button" onClick={reload}>
          Refresh
        </button>
      </PageHead>
      <p class="muted small" style={{ margin: 0 }}>
        A service runs a script or an agent for each request in its queue. Define a service in <code>services/&lt;name&gt;.toml</code> of the config repo or with{" "}
        <code>POST /api/services</code>. Call it with <code>terrarium service call</code>.
      </p>
      <ErrorBanner error={error()} />
      <Show when={loaded()} fallback={<Empty>Loading…</Empty>}>
        <Show when={shown().length} fallback={<Empty>No services. Add a file under services/ in the config repo, then reload.</Empty>}>
          <div class="stack">
            <For each={shown()}>{(s) => <ServiceCard service={s} tick={tick()} />}</For>
          </div>
        </Show>
      </Show>
    </div>
  );
}

/** One service: its definition, the count of each state, and its requests with cancel. */
export function ServiceCard(props: { service: Service; tick?: number }) {
  const { api } = useInstance();
  const s = () => props.service;
  const [state, setState] = createSignal<ServiceRequestState | "">("");
  const [error, setError] = createSignal<unknown>(null);
  const [requests, setRequests] = createStore<ServiceRequest[]>([]);
  const [detail, setDetail] = createSignal<ServiceDetail>();
  const [loadError, setLoadError] = createSignal<unknown>(null);
  // Only the newest load writes, so a slow answer for an old filter is dropped.
  let seq = 0;
  const load = async () => {
    const n = ++seq;
    const name = s().name;
    const st = state();
    try {
      const [rows, d] = await Promise.all([api.services.requests(name, { state: st || undefined, limit: REQUEST_LIMIT }), api.services.get(name).catch(() => undefined)]);
      if (n !== seq) return;
      setRequests(reconcile(rows, { key: "id" }));
      if (d) setDetail(d);
      setLoadError(null);
    } catch (e) {
      if (n === seq) setLoadError(e);
    }
  };
  createEffect(on([() => s().name, state, () => props.tick ?? 0], () => void load()));
  const cancel = async (r: ServiceRequest) => {
    setError(null);
    try {
      await api.services.cancel(r.id);
    } catch (e) {
      setError(e);
    }
    void load();
  };
  return (
    <article class="card-box stack" style={{ gap: "6px" }} aria-label={`Service ${s().name}`}>
      <div class="spread">
        <span class="row" style={{ gap: "6px", "flex-wrap": "wrap" }}>
          <strong class="mono">{s().name}</strong>
          <span class="chip">{s().project ?? "global"}</span>
          <span class="chip">{s().kind}</span>
          <span class="chip" title={s().source === "file" ? `services/${s().name}.toml in the config repo` : "made through the API"}>
            {s().source}
          </span>
          <Show when={s().removed_at}>
            <span class="chip warn" title={`its file went ${s().removed_at}; it takes no new call`}>
              removed
            </span>
          </Show>
          <Show when={s().description}>
            <span class="muted small">{s().description}</span>
          </Show>
        </span>
      </div>
      <dl class="kv small">
        <dt>Target</dt>
        <dd class="mono">{s().target}</dd>
        <dt>Runs</dt>
        <dd class="mono">{runsText(s())}</dd>
        <dt>Resources</dt>
        <dd>{claimText(s().resources)}</dd>
        <dt>Limits</dt>
        <dd>
          {s().concurrency ?? 1} at a time, run at most {s().max_run_time ?? "1h"}, wait at most {s().wait_limit ?? "24h"}, payload at most {s().max_payload ?? "64MiB"}
        </dd>
        <dt>Callers</dt>
        <dd class="mono">{callersText(s())}</dd>
      </dl>
      <ErrorBanner error={error() ?? loadError()} onDismiss={() => setError(null)} />
      <div class="spread small">
        <span class="row" style={{ gap: "6px", "flex-wrap": "wrap" }} aria-label="States">
          <Show when={detail()}>
            {(d) => (
              <span class="chip" title="the open requests of every caller (GET /api/services/{name})">
                {d().open_requests} open in the queue
              </span>
            )}
          </Show>
          <Show when={!state()}>
            <Show when={countsNote(requests.length, REQUEST_LIMIT)}>{(note) => <span class="muted">{note()}</span>}</Show>
            <For each={stateCounts([...requests])} fallback={<span class="muted">No requests.</span>}>
              {(c) => (
                <span class={`chip ${stateTone(c.state)}`}>
                  {stateLabel(c.state)} {c.count}
                </span>
              )}
            </For>
          </Show>
        </span>
        <select aria-label={`State of ${s().name}`} value={state()} onChange={(e) => setState(e.currentTarget.value as ServiceRequestState | "")}>
          <option value="">Every state</option>
          <For each={REQUEST_STATES}>{(st) => <option value={st}>{stateLabel(st)}</option>}</For>
        </select>
      </div>
      <Show when={requests.length}>
        <table class="small" aria-label={`Requests of ${s().name}`}>
          <thead>
            <tr>
              <th>Request</th>
              <th>Caller</th>
              <th>State</th>
              <th>Sent</th>
              <th>Payload</th>
              <th>Outcome</th>
              <th />
            </tr>
          </thead>
          <tbody>
            <For each={requests}>{(r) => <RequestRow request={r} onCancel={() => cancel(r)} />}</For>
          </tbody>
        </table>
      </Show>
    </article>
  );
}

/** One request: a row, and its request JSON, response, and error when opened. */
function RequestRow(props: { request: ServiceRequest; onCancel: () => Promise<void> }) {
  const r = () => props.request;
  const [open, setOpen] = createSignal(false);
  return (
    <>
      <tr>
        <td class="mono">
          <button class="link" type="button" aria-expanded={open()} title={r().id} onClick={() => setOpen(!open())}>
            {r().id.slice(-8)}
          </button>
        </td>
        <td>{r().caller}</td>
        <td>
          <span class={`chip ${stateTone(r().state)}`}>{stateLabel(r().state)}</span>
        </td>
        <td title={r().created_at}>{ago(r().created_at)}</td>
        <td>
          <Show when={r().payload_size} fallback={<span class="muted">none</span>}>
            {bytes(r().payload_size)}
            <Show when={r().payload_deleted_at}>
              {" "}
              <span class="muted">(deleted)</span>
            </Show>
          </Show>
        </td>
        <td>{outcomeText(r())}</td>
        <td>
          <Show when={isOpen(r().state)}>
            <ConfirmButton class="small" label="Cancel request" confirmLabel="Yes, cancel the request" onConfirm={props.onCancel} />
          </Show>
        </td>
      </tr>
      <Show when={open()}>
        <tr>
          <td colSpan={7}>
            <dl class="kv small">
              <dt>ID</dt>
              <dd class="mono">{r().id}</dd>
              <dt>Times</dt>
              <dd>
                sent {r().created_at}
                <Show when={r().started_at}>, started {r().started_at}</Show>
                <Show when={r().ended_at}>, ended {r().ended_at}</Show>
                <Show when={isOpen(r().state)}>, waits until {r().wait_until}</Show>
              </dd>
              <Show when={r().payload_sha256}>
                <dt>Payload sha256</dt>
                <dd class="mono">{r().payload_sha256}</dd>
              </Show>
              <dt>Request</dt>
              <dd>
                <pre class="mono" style={{ margin: 0, "white-space": "pre-wrap" }}>{jsonText(r().request) || "none"}</pre>
              </dd>
              <Show when={jsonText(r().response)}>
                <dt>Response</dt>
                <dd>
                  <pre class="mono" style={{ margin: 0, "white-space": "pre-wrap" }}>{jsonText(r().response)}</pre>
                </dd>
              </Show>
              <Show when={r().error}>
                <dt>Error</dt>
                <dd>{r().error}</dd>
              </Show>
            </dl>
            <p class="muted small" style={{ margin: 0 }}>
              From the CLI: <code>terrarium service status {r().id}</code>.
            </p>
          </td>
        </tr>
      </Show>
    </>
  );
}
