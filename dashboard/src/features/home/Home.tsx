import { A } from "@solidjs/router";
import { createResource, createSignal, For, Match, onCleanup, onMount, Show, Switch } from "solid-js";
import { isLocalOnly, Sparkline, UsageTile } from "../../components/UsageTile";
import { Empty, ErrorBanner, Modal, PageHead, Tabs, useNow } from "../../components/ui";
import type { Subscription, UsageGroup, UsagePoint } from "../../api/types";
import { isMissingEndpoint } from "../../api/http";
import { ago, compact } from "../../lib/format";
import { collectAll } from "../../lib/paging";
import { api, events, projects, subscriptions } from "../../stores";
import { usageLabel } from "../spend/labels";
import { bucketStarts, tokenSeries, toPercent, totalsByKey } from "../spend/split";
import { buildQueue, type ProjectSnapshot } from "./queue";

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

/**
 * Load every project's queue sources. Wiki and versions are not in the daemon
 * yet (docs/api.md §10); a 404 there gives an empty list, and `missing`
 * remembers it so the next refresh does not ask again.
 */
async function loadQueue(missing: Set<string>) {
  const list = projects.projects().length ? projects.projects() : await projects.refresh();
  const optional = async <T,>(what: string, fn: () => Promise<T[]>): Promise<T[]> => {
    if (missing.has(what)) return [];
    try {
      return await fn();
    } catch (e) {
      if (!isMissingEndpoint(e)) throw e;
      missing.add(what);
      return [];
    }
  };
  const snaps: ProjectSnapshot[] = await Promise.all(
    list.map(async (p) => {
      const [versions, pages, board, inbox] = await Promise.all([
        optional("versions", () => api.versions.list(p.name, { status: "in_review" })),
        optional("wiki", () => api.wiki.tree(p.name, { status: "ready_for_review" })),
        api.board.get(p.name),
        collectAll((cursor) => api.inbox.list(p.name, { unread: true, cursor, limit: 1000 }), 5),
      ]);
      return { project: p.name, versions, pages, board, inbox };
    }),
  );
  return buildQueue(snaps);
}

const KIND_LABEL = { version: "version", page: "review", card: "blocked", inbox: "inbox" } as const;
const KIND_CHIP = { version: "info", page: "warn", card: "bad", inbox: "" } as const;

export function Home() {
  const now = useNow(15_000);
  const missing = new Set<string>();
  const [queue, { refetch }] = createResource(() => loadQueue(missing));
  const [detail, setDetail] = createSignal<string | null>(null);
  const strip = useStripUsage(now);

  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (["card", "board", "mail"].includes(e.kind) || /^(version|wiki)\./.test(e.kind)) {
        clearTimeout(t);
        t = setTimeout(refetch, 400);
      }
    });
    onCleanup(() => {
      off();
      clearTimeout(t);
    });
  });

  return (
    <>
      <PageHead title="Home">
        <A class="btn" href="/subscriptions">
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
                    No subscriptions yet. <A href="/subscriptions">Add one</A>.
                  </>
                )}
              </Empty>
            }
          >
            {(s) => <UsageTile sub={s} now={now()} spark={strip.spark(s.name)} tokens24h={strip.total(s.name)} onClick={() => setDetail(s.name)} />}
          </For>
        </div>
      </section>

      <section class="card-box">
        <div class="spread">
          <h2 style={{ margin: 0 }}>Waiting on you</h2>
          <button class="small" onClick={refetch}>
            Refresh
          </button>
        </div>
        <Show when={!queue.error} fallback={<p class="muted">Could not load: {String(queue.error)}</p>}>
          <Show when={queue()?.length} fallback={<p class="muted">{queue.loading ? "Loading…" : "Nothing needs you. Good."}</p>}>
            <div>
              <For each={queue()}>
                {(item) => (
                  <div class="queue-item">
                    <span class="queue-kind">
                      <span class={`chip ${KIND_CHIP[item.kind]}`}>{KIND_LABEL[item.kind]}</span>
                    </span>
                    <div style={{ "min-width": 0, flex: 1 }}>
                      <Switch>
                        <Match when={item.kind === "version" && item}>
                          {(v) => (
                            <>
                              <A href={`/p/${v().project}/versions/${v().version.id}`}>
                                <strong>{v().version.name}</strong>
                              </A>{" "}
                              is in review. <span class="muted">{v().version.notes}</span>
                            </>
                          )}
                        </Match>
                        <Match when={item.kind === "page" && item}>
                          {(p) => (
                            <>
                              <A href={`/p/${p().project}/wiki/${p().page.path}`}>
                                <strong>{p().page.title}</strong>
                              </A>{" "}
                              is ready for review. <span class="muted">by {p().page.author_name ?? p().page.author ?? "someone"}</span>
                            </>
                          )}
                        </Match>
                        <Match when={item.kind === "card" && item}>
                          {(c) => (
                            <>
                              <A href={`/p/${c().project}/board?card=${c().card.id}`}>
                                <strong>{c().card.id}</strong> {c().card.title}
                              </A>
                              <div class="muted small ellipsis">{c().card.comments.at(-1)?.body ?? "Blocked."}</div>
                            </>
                          )}
                        </Match>
                        <Match when={item.kind === "inbox" && item}>
                          {(m) => (
                            <>
                              <A href={`/p/${m().project}/inbox?id=${m().message.id}`}>
                                <strong>{m().message.subject}</strong>
                              </A>{" "}
                              <span class="muted">from {m().message.from_name}</span>
                            </>
                          )}
                        </Match>
                      </Switch>
                    </div>
                    <span class="muted small" style={{ "white-space": "nowrap" }}>
                      {item.project} · {ago(item.at, now())}
                    </span>
                  </div>
                )}
              </For>
            </div>
          </Show>
        </Show>
      </section>

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
      const [project, agent, model] = await Promise.all((["project", "agent", "model"] as const).map((g) => api.usage.series(query(g))));
      return { project: project!, agent: agent!, model: model! };
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
    <Modal title={`${props.sub.plan ?? props.sub.name} (${props.sub.name})`} onClose={() => props.onClose()}>
      <div class="spread">
        <Tabs tabs={RANGES.map((x) => ({ id: x.id, label: x.label }))} active={range()} onChange={setRange} />
        <A class="btn small" href="/subscriptions">
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
      <p class="muted small" style={{ margin: 0 }}>
        The daemon has no per-card usage yet, so this view has no top cards by spend.
      </p>
    </Modal>
  );
}
