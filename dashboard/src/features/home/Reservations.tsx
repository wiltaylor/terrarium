import { useInstance } from "../../instance";
import { createResource, For, Show } from "solid-js";

import { size } from "./hosts";

export function reservationProgress(start: number | null, until: number | null, now: number): number {
  if (start === null || until === null || until <= start) return 0;
  return Math.max(0, Math.min(100, 100 * (now - start) / (until - start)));
}

/** Poll with the host card clock. No cached grant is shown as current after
 * a failed refresh, and unsupported host agents make no resource requests. */
export function HostReservations(props: { host: string; supported: boolean; now: number }) {
  const { api } = useInstance();
  const [data] = createResource(
    () => props.supported ? `${props.host}:${Math.floor(props.now / 10_000)}` : false,
    async () => ({ capacity: await api.hosts.capacity(props.host), rows: await api.hosts.reservations(props.host) }),
  );
  return <Show when={props.supported}>
    <section aria-label="Resource reservations">
      <h3>Reservations</h3>
      <Show when={!data.error} fallback={<p class="muted">Could not load reservations.</p>}>
        <Show when={data()}>{(d) => <>
          <p class="small">Reservable: {d().capacity.percentage}% of this host</p>
          <For each={["cpu_millis", "memory_bytes"] as const}>{(key) => {
            const label = key === "cpu_millis" ? "CPU" : "Memory";
            const display = (n: number) => key === "cpu_millis" ? `${n / 1000} cores` : size(n);
            return <div class="small">
              {label}: {display(d().capacity.used[key])} / {display(d().capacity.reservable[key])}
              <progress aria-label={`${label} reserved capacity`} value={d().capacity.used[key]} max={Math.max(1, d().capacity.reservable[key])} />
            </div>;
          }}</For>
          <ol aria-label="Reservation timeline">
            <For each={d().rows} fallback={<li class="muted">No visible reservations.</li>}>{(r) => <li>
              <span class="mono">{r.holder.id}</span> · {r.state} · {r.exclusive ? "exclusive" : "soft"}
              <Show when={r.until !== null}>
                <div class="small">{r.start === null ? "Pending" : new Date(r.start).toISOString()} → {new Date(r.until!).toISOString()}</div>
                <progress aria-label={`Reservation ${r.id} elapsed`} value={reservationProgress(r.start, r.until, props.now)} max={100} />
              </Show>
              <Show when={r.state === "queued" && r.queue_until !== null}><div class="small">Queue timeout: {new Date(r.queue_until!).toISOString()}</div></Show>
              <Show when={r.reason}><div class="small">{r.reason}</div></Show>
            </li>}</For>
          </ol>
        </>}</Show>
      </Show>
    </section>
  </Show>;
}
