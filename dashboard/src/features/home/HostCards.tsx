// Home, Hosts: one card per host agent with CPU, memory, network IO, disk
// IO, and disk space (docs/api.md §17.1). The page polls every 10 s: the
// daemon sends no event for a sample.

import { A } from "@solidjs/router";
import { createMemo, createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import type { Host, HostMetricsSample } from "../../api/types";
import { isMissingEndpoint } from "../../api/http";
import { Empty } from "../../components/ui";
import { ago } from "../../lib/format";
import { api } from "../../stores";
import {
  CARD_MINUTES,
  clock,
  fullestFirst,
  hostState,
  METRICS,
  nearest,
  pct,
  peak,
  rate,
  scaled,
  series,
  size,
  sortHosts,
  spaceLevel,
  STATE_CHIP,
  timePath,
  timeX,
  type MetricPick,
  type Point,
} from "./hosts";

const POLL_MS = 10_000;

interface HostRow {
  host: Host;
  samples: HostMetricsSample[];
}

/** The hosts and the last 15 minutes of each, again every 10 s. A daemon without the endpoints gives none. */
function useHostMetrics() {
  const [tick, setTick] = createSignal(0);
  let missing = false;
  const [rows] = createResource(tick, async (): Promise<HostRow[]> => {
    const hosts = sortHosts(await api.hosts.list());
    const hist = await Promise.all(
      hosts.map(async (h) => {
        if (missing || !h.capabilities?.metrics) return null;
        try {
          return await api.hosts.metrics(h.name, { minutes: CARD_MINUTES });
        } catch (e) {
          if (isMissingEndpoint(e)) missing = true;
          return null;
        }
      }),
    );
    return hosts.map((host, i) => ({ host, samples: hist[i]?.samples ?? (host.metrics ? [host.metrics] : []) }));
  });
  onMount(() => {
    const t = setInterval(() => setTick((n) => n + 1), POLL_MS);
    onCleanup(() => clearInterval(t));
  });
  return rows;
}

export function HostCards(props: { now: number }) {
  const rows = useHostMetrics();
  return (
    <section aria-label="Hosts" class="host-section">
      <h2>Hosts</h2>
      <Show when={!rows.error || rows.latest} fallback={<p class="muted">Could not load the hosts: {String(rows.error)}</p>}>
        <div class="host-strip">
          <For
            each={rows.latest ?? []}
            fallback={
              <Empty>
                {rows.loading ? (
                  "Loading…"
                ) : (
                  <>
                    No host agents yet. <A href="/settings/hosts">Add one</A>.
                  </>
                )}
              </Empty>
            }
          >
            {(row) => <HostCard host={row.host} samples={row.samples} now={props.now} />}
          </For>
        </div>
      </Show>
    </section>
  );
}

function HostCard(props: { host: Host; samples: HostMetricsSample[]; now: number }) {
  const state = () => hostState(props.host);
  const chip = () => STATE_CHIP[state()];
  const last = () => props.host.metrics ?? props.samples.at(-1);
  // The axis ends at the newest sample, so the line reaches the right edge between polls.
  const until = () => Math.max(Date.parse(last()?.at ?? "") || 0, props.now - 60_000);
  const since = () => until() - CARD_MINUTES * 60_000;
  const [allFs, setAllFs] = createSignal(false);
  const fs = createMemo(() => fullestFirst(last()?.filesystems ?? []));
  const dim = () => state() === "stale" || state() === "offline";
  return (
    <div class="card-box host-card">
      <div class="spread" style={{ "align-items": "flex-start" }}>
        <div style={{ "min-width": 0 }}>
          <A href="/settings/hosts" style={{ "font-weight": 600 }}>
            {props.host.name}
          </A>
          <div class="muted small">
            {[props.host.os, props.host.arch].filter(Boolean).join(" ")}
            <Show when={last()?.cpu}>{(c) => ` · ${c().cores} cores`}</Show>
            <Show when={last()?.memory}>{(m) => ` · ${size(m().total_bytes)}`}</Show>
          </div>
        </div>
        <span class={`chip ${chip().cls}`}>{chip().label}</span>
      </div>

      <Show when={state() === "unsupported"}>
        <p class="muted small">This host agent sends no metrics. Install the new host agent on the host to see them.</p>
      </Show>
      <Show when={state() === "waiting"}>
        <p class="muted small">Waiting for the first sample…</p>
      </Show>
      <Show when={state() === "offline" && !last()}>
        <p class="muted small">Offline. Last seen {ago(props.host.last_seen ?? undefined, props.now)}.</p>
      </Show>

      <Show when={last()}>
        {(l) => (
          <>
            <Show when={dim()}>
              <div class="muted small">Last sample {ago(l().at, props.now)}.</div>
            </Show>
            <div class={`metric-grid${dim() ? " dim" : ""}`}>
              <MetricCell
                label="CPU"
                samples={props.samples}
                pick={METRICS.cpu}
                format={pct}
                max={100}
                since={since()}
                until={until()}
                host={props.host.name}
                sub={l().cpu ? `load ${l().cpu!.load1.toFixed(2)}` : undefined}
              />
              <MetricCell
                label="Memory"
                samples={props.samples}
                pick={METRICS.memory}
                format={pct}
                max={100}
                since={since()}
                until={until()}
                host={props.host.name}
                sub={l().memory ? `${size(l().memory!.used_bytes)} of ${size(l().memory!.total_bytes)}` : undefined}
              />
              <MetricCell label="Network in" samples={props.samples} pick={METRICS.rx} format={rate} since={since()} until={until()} host={props.host.name} />
              <MetricCell label="Network out" samples={props.samples} pick={METRICS.tx} format={rate} since={since()} until={until()} host={props.host.name} />
              <MetricCell label="Disk read" samples={props.samples} pick={METRICS.read} format={rate} since={since()} until={until()} host={props.host.name} />
              <MetricCell label="Disk write" samples={props.samples} pick={METRICS.write} format={rate} since={since()} until={until()} host={props.host.name} />
            </div>
            <Show when={fs().all.length}>
              <div class={`host-fs${dim() ? " dim" : ""}`}>
                <div class="muted small">Disk space</div>
                <For each={allFs() ? fs().all : fs().shown}>
                  {(f) => (
                    <div class="meter" title={`${f.device} (${f.fstype})`}>
                      <div class="meter-label">
                        <span class="mono ellipsis" style={{ color: "var(--text)" }}>
                          {f.mount}
                        </span>
                        <span>
                          <strong style={{ color: "var(--text)" }}>{pct(f.used_pct)}</strong> · {size(f.used_bytes)} of {size(f.total_bytes)}
                          <Show when={spaceLevel(f.used_pct) !== "ok"}> · {spaceLevel(f.used_pct) === "full" ? "almost full" : "filling"}</Show>
                        </span>
                      </div>
                      <div class={`meter-bar ${spaceLevel(f.used_pct)}`} role="progressbar" aria-valuenow={Math.round(f.used_pct)} aria-valuemin={0} aria-valuemax={100} aria-label={`Disk space used on ${f.mount}`}>
                        <span style={{ width: `${Math.min(100, f.used_pct)}%` }} />
                      </div>
                    </div>
                  )}
                </For>
                <Show when={fs().hidden > 0}>
                  <button class="small link-button" onClick={() => setAllFs(!allFs())} aria-expanded={allFs()}>
                    {allFs() ? "Show less" : `${fs().hidden} more`}
                  </button>
                </Show>
              </div>
            </Show>
          </>
        )}
      </Show>
    </div>
  );
}

/** One metric: its name, the current value as text, and a 15-minute trend line with a hover and keyboard readout. */
function MetricCell(props: {
  label: string;
  samples: HostMetricsSample[];
  pick: MetricPick;
  format: (v: number) => string;
  /** The scale top. None: the peak of the series. */
  max?: number;
  since: number;
  until: number;
  host: string;
  sub?: string;
}) {
  const points = createMemo(() => series(props.samples, props.pick, props.since));
  const [hover, setHover] = createSignal<number | null>(null);
  const shown = (): Point | undefined => {
    const i = hover();
    return i !== null ? points()[i] : points().at(-1);
  };
  return (
    <div class="metric-cell">
      <div class="metric-head">
        <span class="muted small">{props.label}</span>
        <span class="metric-value" aria-live={hover() !== null ? "polite" : "off"}>
          {shown() ? props.format(shown()!.value) : "–"}
        </span>
      </div>
      <div class="muted small ellipsis metric-sub">{hover() !== null && shown() ? `at ${clock(shown()!.at)}` : (props.sub ?? (props.max ? "" : points().length ? `peak ${props.format(peak(points()))}` : ""))}</div>
      <TrendLine
        points={points()}
        max={props.max}
        since={props.since}
        until={props.until}
        hover={hover()}
        onHover={setHover}
        label={`${props.label} on ${props.host} over the last ${CARD_MINUTES} minutes${points().length ? `: now ${props.format(points().at(-1)!.value)}, peak ${props.format(peak(points()))}` : ": no samples"}`}
      />
    </div>
  );
}

function TrendLine(props: { points: Point[]; max?: number; since: number; until: number; hover: number | null; onHover: (i: number | null) => void; label: string }) {
  const w = 200;
  const h = 28;
  const ys = createMemo(() => scaled(props.points, props.max));
  const pick = (e: PointerEvent) => {
    const box = (e.currentTarget as SVGElement).getBoundingClientRect();
    if (!box.width || !props.points.length) return;
    const at = props.since + ((e.clientX - box.left) / box.width) * (props.until - props.since);
    props.onHover(nearest(props.points, at));
  };
  const key = (e: KeyboardEvent) => {
    const n = props.points.length;
    if (!n) return;
    const cur = props.hover ?? n - 1;
    if (e.key === "ArrowLeft") props.onHover(Math.max(0, cur - 1));
    else if (e.key === "ArrowRight") props.onHover(Math.min(n - 1, cur + 1));
    else if (e.key === "Home") props.onHover(0);
    else if (e.key === "End" || e.key === "Escape") props.onHover(e.key === "End" ? n - 1 : null);
    else return;
    e.preventDefault();
  };
  return (
    <svg
      class="sparkline metric-spark"
      viewBox={`0 0 ${w} ${h}`}
      preserveAspectRatio="none"
      role="img"
      aria-label={props.label}
      tabIndex={props.points.length ? 0 : undefined}
      onPointerMove={pick}
      onPointerLeave={() => props.onHover(null)}
      onKeyDown={key}
      onBlur={() => props.onHover(null)}
    >
      <line class="metric-base" x1="0" y1={h - 0.5} x2={w} y2={h - 0.5} vector-effect="non-scaling-stroke" />
      <Show when={props.points.length}>
        <path d={timePath(props.points, ys(), props.since, props.until, w, h)} fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" vector-effect="non-scaling-stroke" />
      </Show>
      <Show when={props.hover !== null && props.points[props.hover!]}>
        {(p) => <line class="metric-cross" x1={timeX(p().at, props.since, props.until, w)} y1="0" x2={timeX(p().at, props.since, props.until, w)} y2={h} vector-effect="non-scaling-stroke" />}
      </Show>
    </svg>
  );
}
