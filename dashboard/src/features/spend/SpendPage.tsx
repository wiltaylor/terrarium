// Spend for one project (PLAN §16.2): usage split by agent, by model, and by
// agent and model. `GET /api/usage` is not in the daemon yet; the mock serves it.

import { useParams } from "@solidjs/router";
import { createResource, createSignal, For, Show } from "solid-js";
import { Empty, ErrorBanner, PageHead } from "../../components/ui";
import { compact } from "../../lib/format";
import { api } from "../../stores";
import { splitAgentModel, splitUsage, type SpendRow } from "./split";

const RANGES = [
  { id: "24h", label: "Last 24 hours", hours: 24, bucket: "hour" },
  { id: "7d", label: "Last 7 days", hours: 24 * 7, bucket: "day" },
  { id: "30d", label: "Last 30 days", hours: 24 * 30, bucket: "day" },
] as const;
type RangeId = (typeof RANGES)[number]["id"];

function SpendTable(props: { title: string; rows: SpendRow[] }) {
  const max = () => Math.max(1, ...props.rows.map((r) => r.tokens));
  return (
    <section class="card-box">
      <h3 style={{ "margin-top": 0 }}>{props.title}</h3>
      <Show when={props.rows.length} fallback={<p class="muted">No usage.</p>}>
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Name</th>
                <th style={{ width: "40%" }}>Tokens (input + output)</th>
                <th>Input</th>
                <th>Output</th>
                <th>Cache</th>
                <th>Turns</th>
              </tr>
            </thead>
            <tbody>
              <For each={props.rows}>
                {(r) => (
                  <tr title={`${r.name}: ${r.tokens.toLocaleString()} tokens, ${r.turns} turns`}>
                    <td>{r.name}</td>
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

export default function SpendPage() {
  const params = useParams<{ project: string }>();
  const [range, setRange] = createSignal<RangeId>("24h");
  const [usage] = createResource(
    () => ({ project: params.project, r: RANGES.find((x) => x.id === range())! }),
    ({ project, r }) => api.usage({ project, since: new Date(Date.now() - r.hours * 3600_000).toISOString(), bucket: r.bucket }),
  );
  const total = () => splitUsage(usage() ?? [], "subscription").reduce((n, r) => n + r.tokens, 0);

  return (
    <>
      <PageHead title={`Spend · ${params.project}`}>
        <select value={range()} onChange={(e) => setRange(e.currentTarget.value as RangeId)} aria-label="Range">
          <For each={RANGES}>{(r) => <option value={r.id}>{r.label}</option>}</For>
        </select>
      </PageHead>
      <ErrorBanner error={usage.error} />
      <Show when={!usage.error}>
        <Show when={usage()} fallback={<Empty>Loading…</Empty>}>
          <p class="muted">
            {compact(total())} tokens in this range. Numbers come from local accounting of the harness session files (PLAN §16.2).
          </p>
          <div class="stack">
            <SpendTable title="By agent" rows={splitUsage(usage() ?? [], "agent")} />
            <SpendTable title="By model" rows={splitUsage(usage() ?? [], "model")} />
            <SpendTable title="By agent and model" rows={splitAgentModel(usage() ?? [])} />
            <SpendTable title="By subscription" rows={splitUsage(usage() ?? [], "subscription")} />
          </div>
        </Show>
      </Show>
    </>
  );
}
