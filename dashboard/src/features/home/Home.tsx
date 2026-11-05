import { A } from "@solidjs/router";
import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { isLocalOnly, Sparkline, UsageTile } from "../../components/UsageTile";
import { Empty, ErrorBanner, Modal, PageHead, Tabs, useNow } from "../../components/ui";
import type { Subscription, UsageGroup, UsagePoint } from "../../api/types";
import { ago, compact, subscriptionTitle } from "../../lib/format";
import { api, events, projects, subscriptions } from "../../stores";
import { usageLabel } from "./labels";
import { bucketStarts, cardProject, cardRows, tokenSeries, toPercent, totalsByKey } from "./usage";
import { HostCards } from "./HostCards";
import { ActiveCards } from "./ActiveCards";

const HOUR = 3_600_000;


/** The tokens of each subscription per hour over the last 24 hours (`GET /api/usage?bucket=hour&group=subscription`). */
function useStripUsage(now: () => number) {
  const [tick, setTick] = createSignal(0);
  const [usage] = createResource(tick, () => api.usage.series({ bucket: "hour", group: "subscription", since: new Date(Date.now() - 24 * HOUR).toISOString() }).catch(() => [] as UsagePoint[]));
  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    // `usage.sample` comes at most once each 10 s per agent; batch them.
    const off = events.on((e) => {
      if (e.kind !== "usage.sample" || t) return;
      t = setTimeout(() => {
        t = undefined;
        setTick((n) => n + 1);
      }, 15_000);
    });
    const every = setInterval(() => setTick((n) => n + 1), 5 * 60_000);
    onCleanup(() => {
      off();
      clearTimeout(t);
      clearInterval(every);
    });
  });
  const buckets = () => bucketStarts(now() - 23 * HOUR, now(), "hour");
  return {
    spark: (name: string) => toPercent(tokenSeries(usage.latest ?? [], buckets(), name)),
    total: (name: string) => tokenSeries(usage.latest ?? [], buckets(), name).reduce((a, b) => a + b, 0),
  };
}

export function Home() {
  const now = useNow(15_000);
  const [detail, setDetail] = createSignal<string | null>(null);
  const strip = useStripUsage(now);

  return (
    <>
      <PageHead title="Home">
        <A class="btn" href="/settings/subscriptions">
          Subscriptions
        </A>
      </PageHead>

      <section aria-label="Usage">
        <div class="usage-strip">
          <For
            each={subscriptions.list()}
            fallback={
              <Empty>
                {subscriptions.missing() ? (
                  "Usage is not available yet: the daemon has no subscription endpoints."
                ) : !subscriptions.loaded() ? (
                  "Loading…"
                ) : (
                  <>
                    No subscriptions yet. <A href="/settings/subscriptions">Add one</A>.
                  </>
                )}
              </Empty>
            }
          >
            {(s) => <UsageTile sub={s} now={now()} spark={strip.spark(s.name)} tokens24h={strip.total(s.name)} onClick={() => setDetail(s.name)} />}
          </For>
        </div>
      </section>

      <ActiveCards now={now()} />

      <HostCards now={now()} />

      <Show when={subscriptions.byName(detail() ?? undefined)}>{(s) => <UsageDetail sub={s()} onClose={() => setDetail(null)} />}</Show>
    </>
  );
}

const RANGES = [
  { id: "24h", label: "24 hours", hours: 24, bucket: "hour" },
  { id: "7d", label: "7 days", hours: 24 * 7, bucket: "day" },
] as const;

/** Click-through of a tile: consumption over time, split by project, agent, and model; the probe's raw numbers. */
function UsageDetail(props: { sub: Subscription; onClose: () => void }) {
  const [range, setRange] = createSignal<(typeof RANGES)[number]["id"]>("24h");
  const r = () => RANGES.find((x) => x.id === range())!;
  const query = (group?: UsageGroup) => ({ subscription: props.sub.name, bucket: r().bucket, since: new Date(Date.now() - r().hours * HOUR).toISOString(), group });
  const [series] = createResource(() => [props.sub.name, range()] as const, () => api.usage.series(query()));
  const [split] = createResource(
    () => [props.sub.name, range()] as const,
    async () => {
      const [project, agent, model, card] = await Promise.all((["project", "agent", "model", "card"] as const).map((g) => api.usage.series(query(g))));
      return { project: project!, agent: agent!, model: model!, card: card! };
    },
  );
  const [samples] = createResource(() => props.sub.name, (name) => api.subscriptions.quota(name, { limit: 1000 }));
  /** The newest sample of each window, with the provider's raw object. */
  const latestRaw = () => {
    const m = new Map<string, unknown>();
    for (const q of samples() ?? []) m.set(q.window, { used_pct: q.used_pct, resets_at: q.resets_at, at: q.at, raw: q.raw });
    return Object.fromEntries(m);
  };
  const values = () => {
    const now = Date.now();
    return tokenSeries(series() ?? [], bucketStarts(now - (r().hours - (r().bucket === "hour" ? 1 : 24)) * HOUR, now, r().bucket));
  };
  return (
    <Modal title={`${subscriptionTitle(props.sub)} (${props.sub.name})`} onClose={() => props.onClose()}>
      <div class="spread">
        <Tabs tabs={RANGES.map((x) => ({ id: x.id, label: x.label }))} active={range()} onChange={setRange} />
        <A class="btn small" href="/settings/subscriptions">
          Manage
        </A>
      </div>
      <ErrorBanner error={series.error ?? split.error} />
      <section>
        <div class="spread small muted">
          <span>Tokens (input + output) per {r().bucket}, local accounting</span>
          <span class="mono">{compact(values().reduce((a, b) => a + b, 0))}</span>
        </div>
        <Sparkline values={toPercent(values())} height={56} label={`Tokens per ${r().bucket} on ${props.sub.name}`} />
      </section>
      <div class="grid">
        <For each={["project", "agent", "model"] as const}>
          {(key) => (
            <div>
              <h3>By {key}</h3>
              <table>
                <tbody>
                  <For each={totalsByKey(split()?.[key] ?? [], usageLabel(key))} fallback={<tr><td class="muted">{split.loading ? "Loading…" : "No usage."}</td></tr>}>
                    {(row) => (
                      <tr title={`${row.turns} turns, ${row.messages} responses, ${compact(row.cache)} cache tokens`}>
                        <td>{row.name}</td>
                        <td class="mono">{compact(row.tokens)} tok</td>
                      </tr>
                    )}
                  </For>
                </tbody>
              </table>
            </div>
          )}
        </For>
      </div>
      <section>
        <h3>Top cards by spend</h3>
        <table>
          <tbody>
            <For each={cardRows(split()?.card ?? [], 10)} fallback={<tr><td class="muted">{split.loading ? "Loading…" : "No usage on cards."}</td></tr>}>
              {(row) => {
                const project = () => cardProject(row.key, projects.prefixes());
                return (
                  <tr title={`${row.turns} turns, ${row.messages} responses, ${compact(row.cache)} cache tokens`}>
                    <td>
                      <Show when={project()} fallback={row.name}>
                        {(p) => (
                          <A href={`/p/${p()}/board?card=${encodeURIComponent(row.key)}`} onClick={() => props.onClose()}>
                            {row.name}
                          </A>
                        )}
                      </Show>
                    </td>
                    <td class="mono">{compact(row.tokens)} tok</td>
                  </tr>
                );
              }}
            </For>
          </tbody>
        </table>
      </section>
      <dl class="kv">
        <dt>Provider</dt>
        <dd>{props.sub.provider}</dd>
        <dt>Probe</dt>
        <dd>
          {props.sub.probe ?? "none"} · {props.sub.probe_status}
          <Show when={isLocalOnly(props.sub)}> (local estimate only)</Show>
        </dd>
        <Show when={props.sub.probe_error}>
          <dt>Probe error</dt>
          <dd class="small">{props.sub.probe_error}</dd>
        </Show>
        <dt>Last poll</dt>
        <dd>{props.sub.last_probe_at ? ago(props.sub.last_probe_at) : "never"}</dd>
        <dt>Last success</dt>
        <dd>{props.sub.last_success_at ? ago(props.sub.last_success_at) : "never"}</dd>
        <dt>Alert at</dt>
        <dd>{props.sub.alert_at}%</dd>
      </dl>
      <details>
        <summary>Raw probe numbers</summary>
        <pre>{JSON.stringify(Object.keys(latestRaw()).length ? latestRaw() : props.sub.windows, null, 2)}</pre>
      </details>
    </Modal>
  );
}
