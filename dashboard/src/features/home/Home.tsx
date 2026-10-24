import { A } from "@solidjs/router";
import { createResource, createSignal, For, Match, onCleanup, onMount, Show, Switch } from "solid-js";
import { UsageTile } from "../../components/UsageTile";
import { Empty, Modal, PageHead, useNow } from "../../components/ui";
import type { Subscription } from "../../api/types";
import { isMissingEndpoint } from "../../api/http";
import { ago, compact } from "../../lib/format";
import { collectAll } from "../../lib/paging";
import { api, events, projects, subscriptions } from "../../stores";
import { buildQueue, type ProjectSnapshot } from "./queue";

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
  const [detail, setDetail] = createSignal<Subscription | null>(null);

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
          <For each={subscriptions.list()} fallback={<Empty>{subscriptions.missing() ? "Usage is not available yet: the daemon has no subscription endpoints." : "No subscriptions yet."}</Empty>}>
            {(s) => <UsageTile sub={s} now={now()} onClick={() => setDetail(s)} />}
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

      <Show when={detail()}>{(s) => <UsageDetail sub={s()} onClose={() => setDetail(null)} />}</Show>
    </>
  );
}

function UsageDetail(props: { sub: Subscription; onClose: () => void }) {
  const [usage] = createResource(() => props.sub.name, (name) => api.usage({ subscription: name, bucket: "hour" }));
  const split = (key: "project" | "agent" | "model") => {
    const totals = new Map<string, number>();
    for (const u of usage() ?? []) totals.set(u[key], (totals.get(u[key]) ?? 0) + u.input_tokens + u.output_tokens);
    return [...totals.entries()].sort((a, b) => b[1] - a[1]);
  };
  return (
    <Modal title={`${props.sub.plan} (${props.sub.name})`} onClose={() => props.onClose()}>
      <dl class="kv">
        <dt>Provider</dt>
        <dd>{props.sub.provider}</dd>
        <dt>Probe</dt>
        <dd>{props.sub.probe ?? "none (local estimate only)"}</dd>
        <dt>Last poll</dt>
        <dd>{ago(props.sub.last_poll_at)}</dd>
        <dt>Alert at</dt>
        <dd>{props.sub.alert_at}%</dd>
      </dl>
      <div class="grid">
        <For each={["project", "agent", "model"] as const}>
          {(key) => (
            <div>
              <h3>By {key}</h3>
              <table>
                <tbody>
                  <For each={split(key)} fallback={<tr><td class="muted">No usage.</td></tr>}>
                    {([name, tokens]) => (
                      <tr>
                        <td>{name}</td>
                        <td class="mono">{compact(tokens)} tok</td>
                      </tr>
                    )}
                  </For>
                </tbody>
              </table>
            </div>
          )}
        </For>
      </div>
      <details>
        <summary>Raw probe numbers</summary>
        <pre>{JSON.stringify(props.sub.windows, null, 2)}</pre>
      </details>
    </Modal>
  );
}
