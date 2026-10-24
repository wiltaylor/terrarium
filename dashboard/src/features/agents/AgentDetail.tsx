import { A, useNavigate, useParams } from "@solidjs/router";
import { createEffect, createResource, createSignal, For, Match, on, onCleanup, onMount, Show, Switch } from "solid-js";
import { HoldBadge } from "../../components/QuotaBanner";
import { SelfReport } from "../../components/SelfReport";
import { EventHistory } from "../../components/EventHistory";
import { createPagedList } from "../../components/paged";
import { StatusBadge } from "../../components/StatusBadge";
import { Terminal } from "../../components/Terminal";
import { ConfirmButton, ErrorBanner, LoadMore, PageHead, Tabs, useNow } from "../../components/ui";
import { GRANTABLE_ROLES, type Agent, type AgentStatus, type Role } from "../../api/types";
import { ago, compact, timeUntil } from "../../lib/format";
import { agents, api, events, quota } from "../../stores";
import { describeCause } from "../../stores/quota";
import { AgentForm } from "./AgentForm";
import { RoleBadges } from "./AgentsPage";

const TABS = [
  { id: "terminal", label: "Terminal" },
  { id: "events", label: "Events" },
  { id: "sessions", label: "Sessions" },
] as const;

/** Statuses with a live session: the PTY can attach, `stop` works. */
export const RUNNING: AgentStatus[] = ["starting", "idle", "working", "blocked", "restarting"];

export default function AgentDetail() {
  const params = useParams<{ id: string }>();
  const navigate = useNavigate();
  const id = () => decodeURIComponent(params.id);
  const [tab, setTab] = createSignal<(typeof TABS)[number]["id"]>("terminal");
  const [error, setError] = createSignal<unknown>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [turn, setTurn] = createSignal("");
  const [editing, setEditing] = createSignal(false);
  const [fetched] = createResource(id, (key) => api.agents.get(key).catch((e: unknown) => (setError(e), null)));
  const agent = (): Agent | undefined => agents.byId(id()) ?? agents.byId(fetched()?.id ?? "") ?? fetched() ?? undefined;
  const running = () => RUNNING.includes(agent()?.status ?? "stopped");

  const sessions = createPagedList(
    () => (tab() === "sessions" ? agent()?.id : null),
    (aid, cursor) => api.agents.sessions(aid, { cursor }),
    (s) => s.id,
  );
  // A new session shows up as a status change; reload the list if it is open.
  createEffect(on(() => agent()?.session, () => tab() === "sessions" && void sessions.reload(), { defer: true }));

  const act = async (label: string, fn: () => Promise<unknown>) => {
    try {
      await fn();
      setError(null);
      // 202: the body shows the old status. The events stream brings the change.
      setNotice(`${label} requested. The status follows from the event stream.`);
      setTimeout(() => setNotice(null), 4000);
    } catch (e) {
      setError(e);
    }
  };

  const remove = async (a: Agent) => {
    try {
      await api.agents.remove(a.id, true);
      agents.drop(a.id);
      navigate("/agents");
    } catch (e) {
      setError(e);
    }
  };

  return (
    <Show when={agent()} fallback={<Show when={error()} fallback={<p class="muted">Loading…</p>}><ErrorBanner error={error()} /></Show>}>
      {(a) => (
        <div class="stack">
          <PageHead
            title={
              <span class="row">
                {a().name} <StatusBadge status={a().status} /> <HoldBadge agentId={a().id} long />
              </span>
            }
          >
            <button onClick={() => act("Start", () => api.agents.start(a().id))} disabled={running()}>
              Start
            </button>
            <button onClick={() => act("Restart", () => api.agents.restart(a().id))}>Restart</button>
            <button class="danger" onClick={() => act("Stop", () => api.agents.stop(a().id))} disabled={!running()}>
              Stop
            </button>
            <button onClick={() => setEditing(true)}>Edit</button>
            <ConfirmButton label="Delete" confirmLabel={running() ? "Yes, stop and delete" : "Yes, delete"} onConfirm={() => remove(a())} />
          </PageHead>
          <ErrorBanner error={error()} onDismiss={() => setError(null)} />
          <Show when={notice()}>
            <p class="notice small" style={{ margin: 0 }}>
              {notice()}
            </p>
          </Show>

          <div class="card-box">
            <dl class="kv">
              <dt>Project</dt>
              <dd>{a().project}</dd>
              <dt>Harness</dt>
              <dd>
                {a().harness}
                {a().model ? ` · ${a().model}` : ""}
              </dd>
              <dt>Says</dt>
              <dd>
                <Show when={a().self_report} fallback={<span class="muted">no self-report</span>}>
                  <SelfReport report={a().self_report} />
                </Show>
              </dd>
              <dt>Subscription</dt>
              <dd>
                <Show when={a().subscription} fallback={<span class="muted">none</span>}>
                  {(s) => <A href="/subscriptions">{s()}</A>}
                </Show>
              </dd>
              <dt>Prompt</dt>
              <dd>{a().prompt ?? <span class="muted">default</span>}</dd>
              <dt>Restart</dt>
              <dd>{a().restart ?? "on-failure"}</dd>
              <dt>Policy</dt>
              <dd class="small">
                targets: {a().policy?.targets.join(", ") || "none"} · mail to: {a().policy?.mail_to.join(", ") || "none"} · cross-project:{" "}
                {(a().policy?.cross_project ?? []).join(", ") || "none"}
              </dd>
              <dt>Card</dt>
              <dd>
                <Show when={a().current_card} fallback={<span class="muted">none</span>}>
                  {(c) => <A href={`/p/${a().project}/board?card=${c()}`}>{c()}</A>}
                </Show>
              </dd>
              <dt>Session</dt>
              <dd class="mono small">{a().session ?? "—"}</dd>
              <dt>Workspace</dt>
              <dd class="mono small">{a().workspace}</dd>
              <dt>ID</dt>
              <dd class="mono small">
                {a().id} <Show when={a().source === "file"}><span class="chip">file</span></Show>
              </dd>
            </dl>
          </div>

          <Show when={a().subscription}>
            <QuotaPanel agent={a()} />
          </Show>

          <RolesPanel agent={a()} onError={setError} />

          <form
            class="row"
            onSubmit={(e) => {
              e.preventDefault();
              const text = turn().trim();
              if (!text) return;
              void act("Turn", () => api.agents.send(a().id, text)).then(() => setTurn(""));
            }}
          >
            <input style={{ flex: 1 }} placeholder={running() ? "Type a turn for this agent…" : "Start the agent to send a turn"} value={turn()} onInput={(e) => setTurn(e.currentTarget.value)} aria-label="Turn text" />
            <button class="primary" type="submit" disabled={!running()}>
              Send turn
            </button>
          </form>

          <div>
            <Tabs tabs={TABS} active={tab()} onChange={setTab} />
            <Switch>
              <Match when={tab() === "terminal"}>
                <Terminal connect={() => api.agents.pty(a().id)} available={running()} title={`${a().name} · ${a().harness}`} />
              </Match>
              <Match when={tab() === "events"}>
                <EventHistory filterKey={a().id} fetch={(q) => api.agents.events(a().id, q)} match={(e) => e.agent_id === a().id || e.data?.agent_id === a().id} />
              </Match>
              <Match when={tab() === "sessions"}>
                <ErrorBanner error={sessions.error()} />
                <div class="table-wrap">
                  <table>
                    <thead>
                      <tr>
                        <th>Harness session</th>
                        <th>Started</th>
                        <th>Ended</th>
                        <th>Outcome</th>
                        <th>Turns</th>
                      </tr>
                    </thead>
                    <tbody>
                      <For each={sessions.items()}>
                        {(s) => (
                          <tr>
                            <td class="mono small">
                              {s.harness_session_id ?? s.id} {s.current ? <span class="chip ok">current</span> : null}
                            </td>
                            <td>{ago(s.started_at)}</td>
                            <td>{s.ended_at ? ago(s.ended_at) : "—"}</td>
                            <td>{s.outcome ?? "—"}</td>
                            <td>{s.turns}</td>
                          </tr>
                        )}
                      </For>
                    </tbody>
                  </table>
                  <LoadMore hasMore={sessions.hasMore()} loading={sessions.loading()} onMore={sessions.loadMore} />
                </div>
              </Match>
            </Switch>
          </div>

          <Show when={editing()}>
            <AgentForm agent={a()} onClose={() => setEditing(false)} onSaved={() => setNotice("Saved.")} />
          </Show>
        </div>
      )}
    </Show>
  );
}

/** `GET` and `PUT /api/agents/{id}/roles` (docs/api.md §6.4). */
function RolesPanel(props: { agent: Agent; onError: (e: unknown) => void }) {
  const [roles, { mutate }] = createResource(
    () => props.agent.id,
    (aid) => api.agents.roles(aid).catch((e: unknown) => (props.onError(e), props.agent.roles)),
  );
  const [draft, setDraft] = createSignal<Role[] | null>(null);
  const current = () => draft() ?? roles() ?? props.agent.roles;
  const dirty = () => draft() !== null && [...draft()!].sort().join() !== [...(roles() ?? [])].sort().join();

  const save = async () => {
    try {
      const next = await api.agents.setRoles(props.agent.id, draft() ?? []);
      mutate(next);
      setDraft(null);
      agents.upsert({ ...props.agent, roles: next });
    } catch (e) {
      props.onError(e);
    }
  };

  return (
    <section class="card-box">
      <div class="spread">
        <h3 style={{ margin: 0 }}>Roles</h3>
        <span class="row">
          <RoleBadges agent={{ ...props.agent, roles: roles() ?? props.agent.roles }} />
          <Show when={(roles() ?? props.agent.roles).length === 0}>
            <span class="muted small">member</span>
          </Show>
        </span>
      </div>
      <div class="row" style={{ "flex-wrap": "wrap" }}>
        <For each={GRANTABLE_ROLES}>
          {(r) => (
            <label class="row small">
              <input
                type="checkbox"
                checked={current().includes(r)}
                onChange={(e) => setDraft(e.currentTarget.checked ? [...new Set([...current(), r])] : current().filter((x) => x !== r))}
              />
              {r}
              <span class="muted">{r === "project_admin" ? `(of ${props.agent.project})` : "(all projects)"}</span>
            </label>
          )}
        </For>
        <button class="small primary" disabled={!dirty()} onClick={save}>
          Save roles
        </button>
        <Show when={dirty()}>
          <button class="small" onClick={() => setDraft(null)}>
            Reset
          </button>
        </Show>
      </div>
    </section>
  );
}

/** The agent's subscription, its share today, and any hold (`GET /api/usage/me?agent=`, docs/api.md §16.3). */
function QuotaPanel(props: { agent: Agent }) {
  const now = useNow(30_000);
  const [tick, setTick] = createSignal(0);
  const [me] = createResource(
    () => [props.agent.id, tick()] as const,
    async ([id]) => {
      const m = await api.usage.me(id).catch(() => null);
      // Holds older than the event backlog come from here.
      if (m) quota.seedHolds(id, m.holds, props.agent.name);
      return m;
    },
  );
  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      const mine = e.agent_id === props.agent.id || e.data?.agent_id === props.agent.id;
      if (!(mine && (e.kind.startsWith("quota.") || e.kind === "usage.sample")) && e.kind !== "quota.sample") return;
      clearTimeout(t);
      t = setTimeout(() => setTick((n) => n + 1), 2000);
    });
    onCleanup(() => {
      off();
      clearTimeout(t);
    });
  });
  const holds = () => quota.holdsFor(props.agent.id, now());
  return (
    <section class="card-box">
      <div class="spread">
        <h3 style={{ margin: 0 }}>Quota</h3>
        <span class="muted small">{props.agent.subscription}</span>
      </div>
      <For each={holds()}>
        {(h) => (
          <div class="error-banner" role="status">
            <span>
              <strong>Blocked by quota until {h.until ? `${new Date(h.until).toLocaleString()} (in ${timeUntil(h.until, now())})` : "further notice"}.</strong> {describeCause(h.cause)}: {h.reason}
              {h.enforced ? "" : " The supervisor does not hold turns yet, so this is advice only."}
            </span>
          </div>
        )}
      </For>
      <Show when={me()} fallback={<p class="muted small">{me.loading ? "Loading…" : "No usage numbers from the daemon."}</p>}>
        {(m) => (
          <dl class="kv">
            <dt>Plan</dt>
            <dd>
              {m().subscription?.plan ?? m().subscription?.name ?? props.agent.subscription}
              <Show when={m().subscription?.local_estimate_only}> · local estimate only</Show>
            </dd>
            <dt>Windows</dt>
            <dd class="small">
              <Show when={m().subscription?.windows.length} fallback={<span class="muted">no samples</span>}>
                {m()
                  .subscription!.windows.map((w) => `${w.window} ${Math.round(w.used_pct)}%${w.resets_at ? ` (resets in ${timeUntil(w.resets_at, now())})` : ""}`)
                  .join(" · ")}
              </Show>
            </dd>
            <dt>Today</dt>
            <dd class="small">
              {compact(m().today.mine.input_tokens + m().today.mine.output_tokens)} tokens, {m().today.mine.turns} turns
              <Show when={m().today.share_pct !== undefined}> · {m().today.share_pct}% of the subscription today</Show>
            </dd>
          </dl>
        )}
      </Show>
    </section>
  );
}
