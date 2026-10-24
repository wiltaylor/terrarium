// Spend for one project (PLAN §16.2): local accounting split by agent, by
// model, by subscription, and by card, with a daily series.
// `GET /api/usage?project=&group=agent|model|subscription|card&bucket=day` (docs/api.md §16.3).

import { A, useParams } from "@solidjs/router";
import { createResource, createSignal, For, Show } from "solid-js";
import { Empty, ErrorBanner, PageHead } from "../../components/ui";
import type { UsageGroup } from "../../api/types";
import { compact } from "../../lib/format";
import { api } from "../../stores";
import { usageLabel } from "./labels";
import { bucketStarts, cardRows, tokenSeries, totalsByKey, type SpendRow } from "./split";

const DAY = 86_400_000;
const RANGES = [
  { id: "1d", label: "Today and yesterday", days: 2 },
  { id: "7d", label: "Last 7 days", days: 7 },
  { id: "30d", label: "Last 30 days", days: 30 },
] as const;
type RangeId = (typeof RANGES)[number]["id"];

function SpendTable(props: { title: string; rows: SpendRow[]; href?: (row: SpendRow) => string; empty?: string }) {
  const max = () => Math.max(1, ...props.rows.map((r) => r.tokens));
  return (
    <section class="card-box">
      <h3 style={{ "margin-top": 0 }}>{props.title}</h3>
      <Show when={props.rows.length} fallback={<p class="muted">{props.empty ?? "No usage."}</p>}>
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th style={{ width: "40%" }}>Tokens (input + output)</th>
                <th>Input</th>
                <th>Output</th>
                <th>Cache</th>
                <th>Responses</th>
                <th>Turns</th>
              </tr>
            </thead>
            <tbody>
              <For each={props.rows}>
                {(r) => (
                  <tr title={`${r.name}: ${r.tokens.toLocaleString()} tokens, ${r.turns} turns`}>
                    <td>
                      <Show when={props.href} fallback={r.name}>
                        {(href) => <A href={href()(r)}>{r.name}</A>}
                      </Show>
                    </td>
                    <td>
                      <div class="row" style={{ gap: "8px" }}>
                        <div class="bar-track" style={{ flex: 1 }}>
                          <div class="bar-fill" style={{ width: `${(r.tokens / max()) * 100}%` }} />
                        </div>
                        <span class="mono small">{compact(r.tokens)}</span>
                      </div>
                    </td>
                    <td class="mono small">{compact(r.input)}</td>
                    <td class="mono small">{compact(r.output)}</td>
                    <td class="mono small">{compact(r.cache)}</td>
                    <td class="mono small">{r.messages}</td>
                    <td class="mono small">{r.turns}</td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </Show>
    </section>
  );
}

function DailyBars(props: { days: string[]; values: number[] }) {
  const max = () => Math.max(1, ...props.values);
  return (
    <section class="card-box">
      <h3 style={{ "margin-top": 0 }}>By day (UTC)</h3>
      <div class="day-bars" role="img" aria-label="Tokens per day">
        <For each={props.days}>
          {(d, i) => (
            <div class="day-bar" title={`${d.slice(0, 10)}: ${props.values[i()]!.toLocaleString()} tokens`}>
              <div class="bar-fill" style={{ height: `${(props.values[i()]! / max()) * 100}%` }} />
              <span class="muted small">{d.slice(5, 10)}</span>
            </div>
          )}
        </For>
      </div>
    </section>
  );
}

export default function SpendPage() {
  const params = useParams<{ project: string }>();
  const [range, setRange] = createSignal<RangeId>("7d");
  const [tick, setTick] = createSignal(0);
  const r = () => RANGES.find((x) => x.id === range())!;
  const since = () => new Date(Math.floor(Date.now() / DAY) * DAY - (r().days - 1) * DAY).toISOString();
  const [usage] = createResource(
    () => ({ project: params.project, since: since(), t: tick() }),
    async ({ project, since }) => {
      const q = (group: UsageGroup) => api.usage.series({ project, since, bucket: "day", group });
      const [agent, model, subscription, card] = await Promise.all([q("agent"), q("model"), q("subscription"), q("card")]);
      return { agent: agent!, model: model!, subscription: subscription!, card: card! };
    },
  );
  const days = () => bucketStarts(Date.parse(since()), Date.now(), "day");
  const daily = () => tokenSeries(usage()?.subscription ?? [], days());
  const total = () => daily().reduce((a, b) => a + b, 0);

  return (
    <>
      <PageHead title={`Spend · ${params.project}`}>
        <select value={range()} onChange={(e) => setRange(e.currentTarget.value as RangeId)} aria-label="Range">
          <For each={RANGES}>{(x) => <option value={x.id}>{x.label}</option>}</For>
        </select>
        <button onClick={() => setTick((n) => n + 1)}>Refresh</button>
      </PageHead>
      <ErrorBanner error={usage.error} />
      <Show when={!usage.error}>
        <Show when={usage.latest} fallback={<Empty>Loading…</Empty>}>
          <p class="muted">
            {compact(total())} tokens since {since().slice(0, 10)}. The numbers come from local accounting of the harness session files, by UTC day.
          </p>
          <div class="stack">
            <DailyBars days={days()} values={daily()} />
            <SpendTable title="By agent" rows={totalsByKey(usage.latest?.agent ?? [], usageLabel("agent"))} />
            <SpendTable title="By model" rows={totalsByKey(usage.latest?.model ?? [])} />
            <SpendTable title="By subscription" rows={totalsByKey(usage.latest?.subscription ?? [])} />
            <SpendTable
              title="By card"
              rows={cardRows(usage.latest?.card ?? [])}
              href={(r) => `/p/${params.project}/board?card=${encodeURIComponent(r.key)}`}
              empty="No usage on cards. A sample counts against the card that its agent has in progress."
            />
          </div>
        </Show>
      </Show>
    </>
  );
}
