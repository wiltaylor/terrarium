// The blocks of the project dashboard (docs/api.md §30.4): stat, status,
// chart, text, table, and note. Each monitor block has a head (title and
// state, in a glyph and a word, never colour alone), a body by its kind,
// and a foot with the time of the last result and a stale marker.
//
// The charts follow the dataviz rules: one series in --series-1, a 2 px
// line, a 10 % wash for the min-max band of a bucket, hairline solid grid,
// labelled threshold lines, text in text tokens, a crosshair with a
// readout on hover and on the arrow keys, and a data table.

import { createMemo, createResource, createSignal, For, Match, onCleanup, onMount, Show, Switch } from "solid-js";
import type { DashboardBlock, Monitor, MonitorPoint } from "../../api/types";
import { Markdown } from "../../components/Markdown";
import { ago } from "../../lib/format";
import { renderMarkdown } from "../../lib/markdown";
import { api } from "../../stores";
import { blockState, cellText, columnsOf, numericColumns, linePath, nearestIndex, numericPoints, rangeOf, SPARK_RANGE, STATE_META, timeLabel, unitOf, valueText, xOf, yOf, yScale, type BlockState } from "./model";

/** The state badge: a glyph and a word, with the colour. */
export function StateBadge(props: { state: BlockState; large?: boolean }) {
  const meta = () => STATE_META[props.state];
  return (
    <span class={`mon-badge ${meta().cls}${props.large ? " large" : ""}`}>
      <span aria-hidden="true" class="mon-glyph">
        {meta().glyph}
      </span>
      {meta().label}
    </span>
  );
}

/**
 * The series of a block: the downsampled points of its range. `version` changes with each new result of the
 * monitor, so the series loads again.
 */
function useSeries(monitor: () => Monitor | undefined, range: () => { ms: number; bucket: number }, version: () => number) {
  const [series] = createResource(
    () => {
      const m = monitor();
      return m ? { id: m.id, ms: range().ms, bucket: range().bucket, v: version() } : null;
    },
    async (k) => {
      const since = new Date(Date.now() - k.ms).toISOString();
      return api.monitors.series(k.id, { since, bucket: k.bucket, limit: 2000 });
    },
  );
  return series;
}

/** The body and frame of one block. */
export function BlockView(props: { block: DashboardBlock; monitor?: Monitor; project: string; now: number; version: number }) {
  const b = () => props.block;
  const title = () => b().title || props.monitor?.name || (b().kind === "note" ? "" : "Monitor");
  const state = () => (props.monitor ? blockState(props.monitor, props.now) : "pending");
  return (
    <Switch>
      <Match when={b().kind === "note"}>
        <div class="dash-note">
          <Markdown html={renderMarkdown(b().text ?? "", props.project)} project={props.project} />
        </div>
      </Match>
      <Match when={!props.monitor}>
        <div class="dash-card">
          <div class="dash-head">
            <h3 class="dash-title">{title()}</h3>
          </div>
          <p class="muted small">The monitor of this block is gone. Edit the dashboard to remove the block.</p>
        </div>
      </Match>
      <Match when={props.monitor}>
        {(m) => (
          <div class={`dash-card state-${STATE_META[state()].cls}`}>
            <div class="dash-head">
              <h3 class="dash-title" title={m().description ?? undefined}>
                {title()}
              </h3>
              <StateBadge state={state()} />
            </div>
            <div class="dash-body">
              <Switch>
                <Match when={b().kind === "stat"}>
                  <StatBody block={b()} monitor={m()} version={props.version} />
                </Match>
                <Match when={b().kind === "status"}>
                  <StatusBody monitor={m()} decimals={b().options.decimals} />
                </Match>
                <Match when={b().kind === "chart"}>
                  <ChartBody block={b()} monitor={m()} version={props.version} />
                </Match>
                <Match when={b().kind === "text"}>
                  <Show when={m().last?.text ?? m().last?.json?.message} fallback={<p class="muted small">No text yet.</p>}>
                    {(t) => <pre class="dash-text">{t()}</pre>}
                  </Show>
                </Match>
                <Match when={b().kind === "table"}>
                  <TableBody monitor={m()} />
                </Match>
              </Switch>
            </div>
            <BlockFoot monitor={m()} state={state()} now={props.now} />
          </div>
        )}
      </Match>
    </Switch>
  );
}

function BlockFoot(props: { monitor: Monitor; state: BlockState; now: number }) {
  const last = () => props.monitor.last;
  return (
    <div class="dash-foot muted small">
      <Show when={last()} fallback={<span>{props.monitor.enabled ? "Waiting for the first run" : "The monitor is off"}</span>}>
        {(l) => (
          <span title={l().at}>
            Updated {ago(l().at, props.now)}
            <Show when={props.state === "stale"}>
              <span class="dash-stale"> · stale: expected every {props.monitor.period_secs} s</span>
            </Show>
          </span>
        )}
      </Show>
      <Show when={last()?.error}>{(e) => <span class="dash-error" title={e()}>{e()}</span>}</Show>
    </div>
  );
}

function StatBody(props: { block: DashboardBlock; monitor: Monitor; version: number }) {
  const unit = () => unitOf(props.monitor);
  const showSpark = () => props.block.options.sparkline !== false;
  const series = useSeries(
    () => (showSpark() ? props.monitor : undefined),
    () => SPARK_RANGE,
    () => props.version,
  );
  const th = () => [props.monitor.warn, props.monitor.crit].filter((x) => x !== null);
  return (
    <>
      <div class="dash-stat">
        <span class="dash-value">{valueText(props.monitor.last?.value, props.block.options.decimals, unit())}</span>
      </div>
      <Show when={props.monitor.last?.json?.message ?? (props.monitor.last?.value === undefined ? props.monitor.last?.text : undefined)}>
        {(t) => <div class="muted small ellipsis">{t()}</div>}
      </Show>
      <Show when={th().length}>
        <div class="muted small">
          {props.monitor.warn !== null ? `warn ${props.monitor.direction === "below" ? "≤" : "≥"} ${valueText(props.monitor.warn, undefined, unit())}` : ""}
          {props.monitor.warn !== null && props.monitor.crit !== null ? " · " : ""}
          {props.monitor.crit !== null ? `crit ${props.monitor.direction === "below" ? "≤" : "≥"} ${valueText(props.monitor.crit, undefined, unit())}` : ""}
        </div>
      </Show>
      <Show when={showSpark()}>
        <LineChart points={series.latest ?? []} rangeMs={SPARK_RANGE.ms} bucketMs={SPARK_RANGE.bucket * 1000} unit={unit()} decimals={props.block.options.decimals} name={props.monitor.name} spark loading={series.loading} />
      </Show>
    </>
  );
}

function StatusBody(props: { monitor: Monitor; decimals?: number }) {
  const l = () => props.monitor.last;
  return (
    <div class="dash-status">
      <Show when={l()?.text ?? l()?.json?.message} fallback={<Show when={!l()?.error}><p class="muted small">{l() ? "No message." : "No result yet."}</p></Show>}>
        {(t) => <p class="dash-message">{t()}</p>}
      </Show>
      <Show when={l()?.value !== undefined}>
        <div class="small">
          Value <strong>{valueText(l()?.value, props.decimals, unitOf(props.monitor))}</strong>
        </div>
      </Show>
    </div>
  );
}

function ChartBody(props: { block: DashboardBlock; monitor: Monitor; version: number }) {
  const range = () => rangeOf(props.block);
  const series = useSeries(
    () => props.monitor,
    range,
    () => props.version,
  );
  return (
    <>
      <div class="muted small">
        Last {range().label}, mean of each {range().bucket >= 3600 ? `${range().bucket / 3600} h` : `${range().bucket / 60} min`}
        <Show when={unitOf(props.monitor)}>{(u) => ` (${u()})`}</Show>
      </div>
      <LineChart
        points={series.latest ?? []}
        rangeMs={range().ms}
        bucketMs={range().bucket * 1000}
        unit={unitOf(props.monitor)}
        decimals={props.block.options.decimals}
        warn={props.monitor.warn}
        crit={props.monitor.crit}
        name={props.monitor.name}
        loading={series.loading}
      />
    </>
  );
}

function TableBody(props: { monitor: Monitor }) {
  const rows = () => props.monitor.last?.json?.rows ?? [];
  const cols = createMemo(() => columnsOf(rows()));
  const nums = createMemo(() => numericColumns(rows(), cols()));
  return (
    <Show when={rows().length} fallback={<p class="muted small">{props.monitor.last ? "The last result has no rows." : "No result yet."}</p>}>
      <Show when={props.monitor.last?.json?.message}>{(t) => <div class="muted small">{t()}</div>}</Show>
      <div class="dash-table-wrap">
        <table class="dash-table">
          <thead>
            <tr>
              <For each={cols()}>
                {(c) => (
                  <th scope="col" class={nums().has(c) ? "num" : ""}>
                    {c}
                  </th>
                )}
              </For>
            </tr>
          </thead>
          <tbody>
            <For each={rows()}>
              {(r) => (
                <tr>
                  <For each={cols()}>{(c) => <td class={nums().has(c) ? "num" : ""}>{cellText(r[c])}</td>}</For>
                </tr>
              )}
            </For>
          </tbody>
        </table>
      </div>
      <Show when={props.monitor.last?.json?.truncated}>
        <div class="muted small">The output had more rows or columns; the dashboard keeps 200 rows of 20 columns.</div>
      </Show>
    </Show>
  );
}

/** The width of an element, kept up to date. */
function useWidth(el: () => HTMLElement | undefined, initial = 300) {
  const [w, setW] = createSignal(initial);
  onMount(() => {
    const e = el();
    if (!e) return;
    setW(e.clientWidth || initial);
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(() => setW(e.clientWidth || initial));
    ro.observe(e);
    onCleanup(() => ro.disconnect());
  });
  return w;
}

/**
 * A line chart of one series over `rangeMs` back from now. `spark`: no axes, a short height. The line breaks
 * where buckets are missing. Hover or the arrow keys move a crosshair with a readout.
 */
export function LineChart(props: {
  points: MonitorPoint[];
  rangeMs: number;
  bucketMs: number;
  unit?: string;
  decimals?: number;
  warn?: number | null;
  crit?: number | null;
  name: string;
  spark?: boolean;
  loading?: boolean;
}) {
  let box: HTMLDivElement | undefined;
  const width = useWidth(() => box);
  const [hover, setHover] = createSignal<number | null>(null);
  const pad = () => (props.spark ? { l: 0, r: 0, t: 4, b: 2 } : { l: 44, r: 12, t: 8, b: 22 });
  const height = () => (props.spark ? 36 : 170);
  // The axis ends now; it moves on when the points load again.
  const until = createMemo(() => {
    void props.points;
    return Date.now();
  });
  const since = () => until() - props.rangeMs;
  const pts = createMemo(() => numericPoints(props.points));
  const bands = createMemo(() => props.points.filter((p) => p.min !== undefined && p.max !== undefined && p.min !== p.max));
  const scale = createMemo(() => yScale(props.points, props.spark ? [] : [props.warn, props.crit]));
  const iw = () => Math.max(10, width() - pad().l - pad().r);
  const ih = () => height() - pad().t - pad().b;
  const x = (at: number) => pad().l + xOf(at, since(), until(), iw());
  const y = (v: number) => pad().t + yOf(v, scale().lo, scale().hi, ih());
  const gap = () => props.bucketMs * 2.5;
  const path = () => {
    const p = pad();
    const d = linePath(pts(), since(), until(), scale().lo, scale().hi, iw(), ih(), gap());
    // Move the path into the plot area.
    return { d, transform: `translate(${p.l} ${p.t})` };
  };
  const bandPath = () =>
    bands()
      .map((b) => {
        const at = Date.parse(b.at);
        const x0 = x(at);
        const x1 = Math.min(x(at + props.bucketMs), pad().l + iw());
        return `M${x0.toFixed(1)},${y(b.max!).toFixed(1)} H${x1.toFixed(1)} V${y(b.min!).toFixed(1)} H${x0.toFixed(1)} Z`;
      })
      .join(" ");
  const fmt = (v: number) => valueText(v, props.decimals, props.unit);
  const shown = () => {
    const i = hover();
    return i === null ? undefined : pts()[i];
  };
  const pick = (e: PointerEvent) => {
    const r = (e.currentTarget as SVGElement).getBoundingClientRect();
    if (!pts().length || !r.width) return;
    const at = since() + ((e.clientX - r.left - pad().l) / iw()) * (until() - since());
    setHover(nearestIndex(pts(), at));
  };
  const key = (e: KeyboardEvent) => {
    const n = pts().length;
    if (!n) return;
    const cur = hover() ?? n - 1;
    if (e.key === "ArrowLeft") setHover(Math.max(0, cur - 1));
    else if (e.key === "ArrowRight") setHover(Math.min(n - 1, cur + 1));
    else if (e.key === "Home") setHover(0);
    else if (e.key === "End") setHover(n - 1);
    else if (e.key === "Escape") setHover(null);
    else return;
    e.preventDefault();
  };
  const summary = () => {
    const p = pts();
    if (!p.length) return `${props.name}: no values in the range`;
    const vals = p.map((q) => q.value);
    return `${props.name} over the range: now ${fmt(p.at(-1)!.value)}, low ${fmt(Math.min(...vals))}, high ${fmt(Math.max(...vals))}. Use the arrow keys to read each point.`;
  };
  const xTicks = () => [since(), since() + (until() - since()) / 2, until()];
  const thresholds = () =>
    props.spark
      ? []
      : ([
          ["warn", props.warn],
          ["crit", props.crit],
        ] as const).filter((t): t is readonly ["warn" | "crit", number] => t[1] !== null && t[1] !== undefined && t[1] >= scale().lo && t[1] <= scale().hi);
  return (
    <div class={`dash-chart${props.spark ? " spark" : ""}${props.loading && !props.points.length ? " loading" : ""}`} ref={box}>
      <svg
        width={width()}
        height={height()}
        role="img"
        aria-label={summary()}
        tabIndex={pts().length ? 0 : undefined}
        onPointerMove={pick}
        onPointerLeave={() => setHover(null)}
        onKeyDown={key}
        onBlur={() => setHover(null)}
      >
        <Show when={!props.spark}>
          <For each={scale().ticks}>
            {(t) => (
              <>
                <line class="dash-grid-line" x1={pad().l} x2={pad().l + iw()} y1={y(t)} y2={y(t)} />
                <text class="dash-axis" x={pad().l - 6} y={y(t)} text-anchor="end" dominant-baseline="middle">
                  {valueText(t, undefined, undefined)}
                </text>
              </>
            )}
          </For>
          <For each={xTicks()}>
            {(t, i) => (
              <text class="dash-axis" x={x(t)} y={height() - 6} text-anchor={i() === 0 ? "start" : i() === 2 ? "end" : "middle"}>
                {i() === 2 ? "now" : timeLabel(t, props.rangeMs)}
              </text>
            )}
          </For>
          <For each={thresholds()}>
            {([name, v]) => (
              <>
                <line class={`dash-threshold ${name}`} x1={pad().l} x2={pad().l + iw()} y1={y(v)} y2={y(v)} />
                <text class="dash-axis" x={pad().l + iw() - 2} y={y(v) - 4} text-anchor="end">
                  {name} {fmt(v)}
                </text>
              </>
            )}
          </For>
        </Show>
        <Show when={!props.spark && bands().length}>
          <path class="dash-band" d={bandPath()} />
        </Show>
        <path class="dash-line" d={path().d} transform={path().transform} />
        <Show when={shown()}>
          {(p) => (
            <>
              <line class="dash-cross" x1={x(p().at)} x2={x(p().at)} y1={pad().t} y2={pad().t + ih()} />
              <circle class="dash-dot" cx={x(p().at)} cy={y(p().value)} r="4" />
            </>
          )}
        </Show>
      </svg>
      <Show when={shown()}>
        {(p) => (
          <div class="dash-tip" style={{ left: `${Math.min(Math.max(0, x(p().at) - 60), Math.max(0, width() - 130))}px` }} aria-live="polite">
            <strong>{fmt(p().value)}</strong>
            <span class="muted">
              {new Date(p().at).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })} · {STATE_META[p().status].label}
            </span>
          </div>
        )}
      </Show>
      <Show when={!props.spark}>
        <details class="dash-data">
          <summary class="small muted">Data table</summary>
          <div class="dash-table-wrap">
            <table class="dash-table">
              <thead>
                <tr>
                  <th scope="col">Time</th>
                  <th scope="col">Mean</th>
                  <th scope="col">Low</th>
                  <th scope="col">High</th>
                  <th scope="col">Status</th>
                </tr>
              </thead>
              <tbody>
                <For each={[...props.points].reverse().slice(0, 200)}>
                  {(p) => (
                    <tr>
                      <td>{new Date(p.at).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })}</td>
                      <td class="num">{valueText(p.value, props.decimals)}</td>
                      <td class="num">{valueText(p.min, props.decimals)}</td>
                      <td class="num">{valueText(p.max, props.decimals)}</td>
                      <td>{STATE_META[p.status].label}</td>
                    </tr>
                  )}
                </For>
              </tbody>
            </table>
          </div>
        </details>
      </Show>
    </div>
  );
}
