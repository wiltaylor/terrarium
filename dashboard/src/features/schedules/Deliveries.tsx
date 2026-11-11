import { useInstance } from "../../instance";
// The Deliveries panel of a webhook trigger (docs/api.md §19.5): the
// deliveries, newest first (status chips, HTTP status, time taken, attempt,
// time, the answer), Redeliver, and Send test with the request and the
// answer. The state is in ./deliveries; this file only draws it.

import { For, onCleanup, onMount, Show } from "solid-js";
import type { Schedule } from "../../api/types";
import { ErrorBanner, LoadMore } from "../../components/ui";

import { createDeliveries, touchesDeliveries } from "./deliveries";
import { canRedeliver, DELIVERY_CHIP, deliveryStatusWords, prettyBody } from "./webhook";

export function DeliveriesPanel(props: { schedule: Schedule }) {
  const { api, events } = useInstance();
  // The panel belongs to one schedule; a new schedule mounts a new panel.
  const d = createDeliveries(api, props.schedule.id);
  onMount(() => {
    void d.reload();
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (!touchesDeliveries(e, props.schedule.id)) return;
      clearTimeout(t);
      t = setTimeout(() => void d.syncHead(), 200);
    });
    onCleanup(() => {
      off();
      clearTimeout(t);
    });
  });
  return (
    <div class="stack lease-section" style={{ gap: "6px" }} aria-label={`Deliveries of ${props.schedule.name}`}>
      <div class="spread" style={{ "flex-wrap": "wrap", gap: "8px" }}>
        <span class="small">
          <strong>Deliveries</strong> <span class="muted">· kept 7 days; retries after 10 s, 1 min, 5 min, 30 min, 2 h</span>
          <Show when={props.schedule.webhook_failures}>
            {(n) => (
              <>
                {" "}
                <span class="chip warn" title="The 20th failed attempt in a row disables the trigger">
                  {n()} failed in a row
                </span>
              </>
            )}
          </Show>
        </span>
        <span class="row" style={{ gap: "6px" }}>
          <button class="small" disabled={d.busy() === "test"} onClick={() => void d.sendTest()} title="Send a sample event of the trigger's kind now">
            {d.busy() === "test" ? "Sending…" : "Send test"}
          </button>
          <button class="small ghost" onClick={() => void d.reload()}>
            Refresh
          </button>
        </span>
      </div>
      <ErrorBanner error={d.error()} />
      <Show when={d.test()}>
        {(r) => (
          <div class="card-box hook-test" role="status" aria-label="Test result" style={{ padding: "8px" }}>
            <div class="spread" style={{ gap: "8px", "flex-wrap": "wrap" }}>
              <span class="small">
                <strong>Test send:</strong> <span class={`chip ${DELIVERY_CHIP[r().delivery.status]}`}>{r().delivery.status}</span>
                <Show when={r().delivery.http_status}>{(h) => <span class="mono"> HTTP {h()}</span>}</Show>
                <Show when={r().delivery.duration_ms !== null}> · {r().delivery.duration_ms} ms</Show>
              </span>
              <button class="ghost small" aria-label="Close the test result" onClick={() => d.clearTest()}>
                ✕
              </button>
            </div>
            <Show when={r().delivery.error}>{(e) => <pre class="error-detail run-output">{e()}</pre>}</Show>
            <details>
              <summary class="small muted">request</summary>
              <pre class="run-output mono small">
                {`${r().request.method} ${r().request.url}\n${r()
                  .request.headers.map(([k, v]) => `${k}: ${v}`)
                  .join("\n")}\n\n${prettyBody(r().request.body)}`}
              </pre>
            </details>
            <Show when={r().delivery.response}>
              {(resp) => (
                <details open>
                  <summary class="small muted">response</summary>
                  <pre class="run-output mono small">{resp()}</pre>
                </details>
              )}
            </Show>
          </div>
        )}
      </Show>
      <Show when={d.deliveries().length} fallback={<span class="small muted">{d.initial() || d.loading() ? "Loading…" : "No deliveries yet."}</span>}>
        <div class="table-wrap">
          <table class="small" aria-label={`Delivery log of ${props.schedule.name}`}>
            <thead>
              <tr>
                <th>Time</th>
                <th>Event</th>
                <th>Status</th>
                <th>HTTP</th>
                <th>Took</th>
                <th>Try</th>
                <th>Answer</th>
                <th />
              </tr>
            </thead>
            <tbody>
              <For each={d.deliveries()}>
                {(x) => (
                  <tr aria-label={`Delivery ${x.id}`}>
                    <td title={x.created_at}>{new Date(x.created_at).toLocaleString()}</td>
                    <td>
                      <Show when={x.test}>
                        <span class="chip info">test</span>{" "}
                      </Show>
                      <Show when={x.event_seq}>{(s) => <span class="mono">#{s()} </span>}</Show>
                      <span class="mono">{x.event_kind || "—"}</span>
                      <Show when={x.redelivery_of}>
                        <div class="muted">redelivery</div>
                      </Show>
                    </td>
                    <td>
                      <span class={`chip ${DELIVERY_CHIP[x.status]}`}>{deliveryStatusWords(x)}</span>
                    </td>
                    <td class="mono">{x.http_status ?? "—"}</td>
                    <td>{x.duration_ms === null ? "—" : `${x.duration_ms} ms`}</td>
                    <td>{x.attempt || "—"}</td>
                    <td style={{ "max-width": "22rem" }}>
                      <Show when={x.error}>{(e) => <div class="error-detail ellipsis" title={e()}>{e()}</div>}</Show>
                      <Show when={x.response}>
                        {(resp) => (
                          <details>
                            <summary class="muted ellipsis">{resp().slice(0, 60)}</summary>
                            <pre class="run-output">{resp()}</pre>
                          </details>
                        )}
                      </Show>
                      <Show when={x.request && x.status !== "dropped"}>
                        <details>
                          <summary class="muted">request · {x.request_bytes} B</summary>
                          <pre class="run-output">{prettyBody(x.request)}</pre>
                        </details>
                      </Show>
                    </td>
                    <td>
                      <Show when={canRedeliver(x)}>
                        <button class="small" disabled={d.busy() === x.id} onClick={() => void d.redeliver(x.id)}>
                          {d.busy() === x.id ? "Queuing…" : "Redeliver"}
                        </button>
                      </Show>
                    </td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </Show>
      <LoadMore hasMore={d.hasMore()} loading={d.loading()} onMore={() => void d.loadMore()} count={d.deliveries().length || undefined} />
    </div>
  );
}
