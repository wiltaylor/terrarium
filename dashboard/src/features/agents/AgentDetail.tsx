import { A, useParams } from "@solidjs/router";
import { createMemo, createResource, createSignal, For, Match, Show, Switch } from "solid-js";
import { EventList } from "../../components/EventList";
import { StatusBadge } from "../../components/StatusBadge";
import { Terminal } from "../../components/Terminal";
import { ErrorBanner, PageHead, Tabs } from "../../components/ui";
import { ago } from "../../lib/format";
import { agents, api, events } from "../../stores";
import { RoleBadges } from "./AgentsPage";

const TABS = [
  { id: "terminal", label: "Terminal" },
  { id: "events", label: "Events" },
  { id: "sessions", label: "Sessions" },
] as const;

export default function AgentDetail() {
  const params = useParams<{ id: string }>();
  const [tab, setTab] = createSignal<(typeof TABS)[number]["id"]>("terminal");
  const [error, setError] = createSignal<string | null>(null);
  const [turn, setTurn] = createSignal("");
  const [fetched] = createResource(() => params.id, (id) => api.agents.get(id));
  const agent = () => agents.byId(params.id) ?? fetched();

  const [history] = createResource(() => params.id, (id) => api.agents.events(id));
  const [sessions] = createResource(() => (tab() === "sessions" ? params.id : null), (id) => api.agents.sessions(id));
  const live = createMemo(() => {
    const a = agent();
    if (!a) return [];
    const seen = new Set<string>();
    return [...events.recent().filter((e) => e.agent === a.name && e.project === a.project), ...(history() ?? [])].filter((e) =>
      seen.has(e.id) ? false : (seen.add(e.id), true),
    );
  });

  const act = async (fn: () => Promise<void>) => {
    try {
      await fn();
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  };

  return (
    <Show when={agent()} fallback={<p class="muted">{fetched.error ? `Agent not found: ${params.id}` : "Loading…"}</p>}>
      {(a) => (
        <div class="stack">
          <PageHead
            title={
              <span class="row">
                {a().name} <StatusBadge status={a().status} />
              </span>
            }
          >
            <button onClick={() => act(() => api.agents.start(a().id))} disabled={!["stopped", "error"].includes(a().status)}>
              Start
            </button>
            <button onClick={() => act(() => api.agents.restart(a().id))}>Restart</button>
            <button class="danger" onClick={() => act(() => api.agents.stop(a().id))} disabled={a().status === "stopped"}>
              Stop
            </button>
          </PageHead>
          <ErrorBanner error={error()} onDismiss={() => setError(null)} />

          <div class="card-box">
            <dl class="kv">
              <dt>Project</dt>
              <dd>{a().project}</dd>
              <dt>Harness</dt>
              <dd>
                {a().harness}
                {a().model ? ` · ${a().model}` : ""}
              </dd>
              <dt>Subscription</dt>
              <dd>{a().subscription}</dd>
              <dt>Card</dt>
              <dd>
                <Show when={a().current_card} fallback={<span class="muted">none</span>}>
                  {(c) => <A href={`/p/${a().project}/board?card=${c()}`}>{c()}</A>}
                </Show>
              </dd>
              <dt>Roles</dt>
              <dd class="row">
                <RoleBadges agent={a()} />
                <Show when={a().roles.every((r) => r === "member")}>
                  <span class="muted">member</span>
                </Show>
              </dd>
              <dt>Workspace</dt>
              <dd class="mono small">{a().workspace}</dd>
              <dt>ID</dt>
              <dd class="mono small">{a().id}</dd>
            </dl>
          </div>

          <form
            class="row"
            onSubmit={(e) => {
              e.preventDefault();
              const text = turn().trim();
              if (!text) return;
              void act(() => api.agents.send(a().id, text)).then(() => setTurn(""));
            }}
          >
            <input style={{ flex: 1 }} placeholder="Type a turn for this agent…" value={turn()} onInput={(e) => setTurn(e.currentTarget.value)} aria-label="Turn text" />
            <button class="primary" type="submit">
              Send turn
            </button>
          </form>

          <div>
            <Tabs tabs={TABS} active={tab()} onChange={setTab} />
            <Switch>
              <Match when={tab() === "terminal"}>
                <Terminal connect={() => api.agents.pty(a().id)} title={`${a().name} · ${a().harness}`} />
              </Match>
              <Match when={tab() === "events"}>
                <EventList events={live()} />
              </Match>
              <Match when={tab() === "sessions"}>
                <div class="table-wrap">
                  <table>
                    <thead>
                      <tr>
                        <th>Harness session</th>
                        <th>Started</th>
                        <th>Ended</th>
                        <th>Turns</th>
                      </tr>
                    </thead>
                    <tbody>
                      <For each={sessions()}>
                        {(s) => (
                          <tr>
                            <td class="mono small">
                              {s.harness_session_id} {s.current ? <span class="chip ok">current</span> : null}
                            </td>
                            <td>{ago(s.started_at)}</td>
                            <td>{s.ended_at ? ago(s.ended_at) : "—"}</td>
                            <td>{s.turns}</td>
                          </tr>
                        )}
                      </For>
                    </tbody>
                  </table>
                </div>
              </Match>
            </Switch>
          </div>
        </div>
      )}
    </Show>
  );
}
