import { A, useNavigate, useParams, useSearchParams } from "@solidjs/router";
import { createResource, createSignal, For, Match, onCleanup, onMount, Show, Switch } from "solid-js";
import { GrantDialog, grantNotice, LeaseChips, LeaseTable, useLeaseList } from "../../components/Leases";
import { HoldBadge } from "../../components/QuotaBanner";
import { SelfReport } from "../../components/SelfReport";
import { EventHistory } from "../../components/EventHistory";
import { StatusBadge } from "../../components/StatusBadge";
import { Terminal } from "../../components/Terminal";
import { ConfirmButton, ErrorBanner, PageHead, Tabs, useNow } from "../../components/ui";
import { GRANTABLE_ROLES, type Agent, type AgentStatus, type Role } from "../../api/types";
import { compact, timeUntil, windowValue } from "../../lib/format";
import { agents, api, events, leases as holds, quota, subscriptions } from "../../stores";
import { describeCause } from "../../stores/quota";
import { eventAgentId } from "../../stores/agents";
import { ApiError } from "../../api/http";
import { AgentForm } from "./AgentForm";
import { SaveAsTemplateDialog } from "../templates/TemplateDialog";
import { Conversation } from "./conversation/Conversation";
import { ScreenPanel } from "./ScreenPanel";
import { SessionsPanel } from "./SessionsPanel";
import { AGENT_TABS, agentViewQuery, parseAgentView, type AgentTab } from "./sessions";
import { RoleBadges } from "./AgentsPage";
import { appliedText, changeSubscription, loginText, noSubscriptionAccount, noSubscriptionAccountText, normalizePool, samePool, swapTargets } from "./subscription";
import { PoolPicker, SubscriptionSelect } from "./SubscriptionFields";
import { SystemPromptField } from "./AgentFields";
import { effectivePersona, systemPromptProblem } from "./form";
import { WaitsPanel, WatchesPanel } from "./Waits";
import { QuestionsPanel } from "../inbox/QuestionsPanel";
import { SkillsPicker } from "../skills/SkillsPicker";
import { RESTART_NOTE, skillsLabel } from "../skills/model";

/** The page of an agent that the daemon removed. */
function RemovedAgent() {
  return (
    <div class="stack">
      <PageHead title="Agent removed" />
      <p class="muted">
        This agent was removed. <A href="/agents">Back to the agents</A>
      </p>
    </div>
  );
}

/** Statuses with a live session: the PTY can attach, `stop` works. */
export const RUNNING: AgentStatus[] = ["starting", "idle", "waiting", "working", "blocked", "restarting"];

export default function AgentDetail() {
  const params = useParams<{ id: string }>();
  const navigate = useNavigate();
  const id = () => decodeURIComponent(params.id);
  // The tab and the open session are in the URL (`?tab=sessions&session=<id>`), so a reload keeps them.
  const [search, setSearch] = useSearchParams<{ tab?: string; session?: string }>();
  const view = () => parseAgentView({ tab: search.tab, session: search.session });
  const tab = () => view().tab;
  const setTab = (t: AgentTab) => setSearch(agentViewQuery({ tab: t, session: null }));
  const openSession = (session: string | null) => setSearch(agentViewQuery({ tab: "sessions", session }));
  const [error, setError] = createSignal<unknown>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [turn, setTurn] = createSignal("");
  const [editing, setEditing] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  const [fetched] = createResource(id, (key) => api.agents.get(key).catch((e: unknown) => (setError(e), null)));
  const agent = (): Agent | undefined => agents.byId(id()) ?? agents.byId(fetched()?.id ?? "") ?? fetched() ?? undefined;
  const running = () => RUNNING.includes(agent()?.status ?? "stopped");
  // The daemon removed the agent while this page shows it (an `agent_removed`
  // event), or the GET found no agent (404): the page says so, not a raw error.
  const [removedKey, setRemovedKey] = createSignal<string | null>(null);
  const notFound = () => {
    const e = error();
    return e instanceof ApiError && e.status === 404 && !agent();
  };
  const removed = () => removedKey() === id() || notFound();
  onMount(() => {
    const off = events.on((e) => {
      const gone = e.kind === "agent_removed" ? eventAgentId(e) : undefined;
      if (gone && gone === (agent()?.id ?? fetched()?.id)) setRemovedKey(id());
    });
    onCleanup(off);
  });

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
    <Show
      when={!removed() && agent()}
      fallback={
        <Show when={!removed()} fallback={<RemovedAgent />}>
          <Show when={error()} fallback={<p class="muted">Loading…</p>}>
            <ErrorBanner error={error()} />
          </Show>
        </Show>
      }
    >
      {(a) => (
        <div class="stack">
          <PageHead
            title={
              <span class="row">
                {a().name} <StatusBadge status={a().status} waitingOn={a().waiting_on} /> <HoldBadge agentId={a().id} long /> <LeaseChips agentId={a().id} />
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
            <button onClick={() => setSaving(true)}>Save as template</button>
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
                <SubscriptionCell agent={a()} running={running()} onError={setError} />
              </dd>
              <dt>Pool</dt>
              <dd>
                <PoolCell agent={a()} onError={setError} />
              </dd>
              <dt>Prompt</dt>
              <dd>{a().prompt ?? <span class="muted">default</span>}</dd>
              <dt>System prompt</dt>
              <dd>
                <SystemPromptCell agent={a()} running={running()} onError={setError} />
              </dd>
              <dt>Persona</dt>
              <dd>
                {effectivePersona(a()).persona}
                <Show when={effectivePersona(a()).byRole}>
                  <span class="muted"> (by role)</span>
                </Show>
                <Show when={a().system_prompt}>
                  <span class="muted small"> · the system prompt wins</span>
                </Show>
              </dd>
              <dt>Skills</dt>
              <dd>
                <SkillsCell agent={a()} running={running()} onError={setError} />
              </dd>
              <dt>Restart</dt>
              <dd>{a().restart ?? "on-failure"}</dd>
              <Show when={a().template}>
                {(t) => (
                  <>
                    <dt>Template</dt>
                    <dd>
                      <A href="/templates">{t().name}</A> <span class="chip">{t().project ?? "global"}</span> <span class="muted small">revision {t().revision} at the create; a copy, not a live link</span>
                    </dd>
                  </>
                )}
              </Show>
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

          <LeasesPanel agent={a()} onNotice={(n) => setNotice(n)} />

          <QuestionsPanel agent={a()} />

          <WaitsPanel agent={a()} />

          <WatchesPanel agent={a()} />

          {/* The Conversation tab has its own composer. */}
          <Show when={tab() !== "conversation"}>
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
          </Show>

          <div>
            <Tabs tabs={AGENT_TABS} active={tab()} onChange={setTab} />
            <Switch>
              <Match when={tab() === "conversation"}>
                <Conversation agent={a()} onOpenTerminal={() => setTab("terminal")} />
              </Match>
              <Match when={tab() === "terminal"}>
                <Terminal connect={() => api.agents.pty(a().id)} available={running()} title={`${a().name} · ${a().harness}`} />
              </Match>
              <Match when={tab() === "screen"}>
                <ScreenPanel agentId={a().id} running={running()} />
              </Match>
              <Match when={tab() === "events"}>
                <EventHistory filterKey={a().id} fetch={(q) => api.agents.events(a().id, q)} match={(e) => e.agent_id === a().id || e.data?.agent_id === a().id} />
              </Match>
              <Match when={tab() === "sessions"}>
                <SessionsPanel agent={a()} open={view().session} onOpen={openSession} onOpenConversation={() => setTab("conversation")} />
              </Match>
            </Switch>
          </div>

          <Show when={editing()}>
            <AgentForm agent={a()} onClose={() => setEditing(false)} onSaved={() => setNotice("Saved.")} />
          </Show>
          <Show when={saving()}>
            <SaveAsTemplateDialog agent={a()} onClose={() => setSaving(false)} onSaved={(t) => setNotice(`Saved template ${t.project ?? "global"}/${t.name}.`)} />
          </Show>
        </div>
      )}
    </Show>
  );
}

/**
 * The agent's subscription (`subscription_detail` of `GET /api/agents/{id}`)
 * with its login status, and a control to change it with
 * `PATCH /api/agents/{id}`. The change applies at the next start.
 */
export function SubscriptionCell(props: { agent: Agent; running: boolean; onError: (e: unknown) => void }) {
  const [detail] = createResource(
    () => [props.agent.id, props.agent.subscription ?? ""] as const,
    async ([id, name]) => {
      if (!name) return undefined;
      const got = await api.agents.get(id).catch(() => null);
      if (got?.subscription_detail?.name === name) return got.subscription_detail;
      // An older daemon has no subscription_detail: use the list.
      await subscriptions.ensureLoaded();
      const s = subscriptions.byName(name);
      return s ? { name: s.name, provider: s.provider, plan: s.plan, login_status: s.login_status, harnesses: s.harnesses, default_model: s.default_models?.[props.agent.harness] } : undefined;
    },
  );
  const [editing, setEditing] = createSignal(false);
  const [draft, setDraft] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [saved, setSaved] = createSignal(false);
  const fileManaged = () => props.agent.source === "file";
  // PLAN §16.3: with no subscription, the default account of the harness.
  const [fallback] = createResource(
    () => (props.agent.subscription ? null : props.agent.harness),
    async (harness) => {
      await subscriptions.ensureLoaded();
      return noSubscriptionAccount(subscriptions.list(), harness);
    },
  );

  const save = async () => {
    setBusy(true);
    try {
      const a = await changeSubscription(api, props.agent.id, draft());
      agents.upsert(a);
      setEditing(false);
      setSaved(true);
    } catch (e) {
      props.onError(e);
    } finally {
      setBusy(false);
    }
  };
  const restart = async () => {
    try {
      await api.agents.restart(props.agent.id);
      setSaved(false);
    } catch (e) {
      props.onError(e);
    }
  };

  return (
    <div class="stack" style={{ gap: "4px" }}>
      <Show
        when={!editing()}
        fallback={
          <span class="row" style={{ "flex-wrap": "wrap" }}>
            <SubscriptionSelect harness={props.agent.harness} value={draft()} onChange={setDraft} label="New subscription" />
            <button class="small primary" disabled={busy() || draft() === (props.agent.subscription ?? "")} onClick={save}>
              Save
            </button>
            <button class="small" onClick={() => setEditing(false)}>
              Cancel
            </button>
          </span>
        }
      >
        <span class="row" style={{ "flex-wrap": "wrap" }}>
          <Show
            when={props.agent.subscription}
            fallback={
              <>
                <span class="muted">none</span>
                <Show when={fallback()}>
                  {(f) => <span class={"error" in f() ? "chip warn" : "muted small"}>{noSubscriptionAccountText(f())}</span>}
                </Show>
              </>
            }
          >
            {(name) => (
              <>
                <A href="/subscriptions">{name()}</A>
                <Show when={detail()} fallback={<Show when={!detail.loading}><span class="chip warn">not found</span></Show>}>
                  {(d) => (
                    <>
                      <span class="muted small">
                        {d().provider}
                        {d().plan ? ` · ${d().plan}` : ""}
                      </span>
                      <span class={`chip ${d().login_status === "expired" || d().login_status === "missing" ? "warn" : "ok"}`}>{loginText(d().login_status)}</span>
                      <Show when={!d().harnesses.includes(props.agent.harness)}>
                        <span class="chip warn">not for {props.agent.harness}</span>
                      </Show>
                      <Show when={!props.agent.model && d().default_model}>
                        <span class="muted small">model {d().default_model} (default)</span>
                      </Show>
                    </>
                  )}
                </Show>
              </>
            )}
          </Show>
          <button
            class="small"
            disabled={fileManaged()}
            title={fileManaged() ? "A file agent: edit its definition file." : undefined}
            onClick={() => {
              setDraft(props.agent.subscription ?? "");
              setSaved(false);
              setEditing(true);
            }}
          >
            Edit
          </button>
        </span>
      </Show>
      <Show when={editing() || saved()}>
        <span class="muted small">
          A new subscription applies at the next start.
          <Show when={saved() && props.running}>
            {" "}
            <button class="small" onClick={restart}>
              Restart now
            </button>
          </Show>
        </span>
      </Show>
    </div>
  );
}

/**
 * The agent's subscription pool (PLAN §16.3): edit it with
 * `PATCH /api/agents/{id}`, and swap to another account of it with
 * `POST /api/agents/{id}/subscription`. A file agent cannot edit its pool
 * here, but it can swap inside it.
 */
export function PoolCell(props: { agent: Agent; onError: (e: unknown) => void }) {
  const [editing, setEditing] = createSignal(false);
  const [draft, setDraft] = createSignal<string[]>([]);
  const [target, setTarget] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [result, setResult] = createSignal<string | null>(null);
  const fileManaged = () => props.agent.source === "file";
  const pool = () => props.agent.subscription_pool ?? [];
  const targets = () => swapTargets(props.agent);
  const current = () => props.agent.subscription ?? "";

  const save = async () => {
    setBusy(true);
    try {
      const a = await api.agents.update(props.agent.id, { subscription_pool: normalizePool(draft(), current()) });
      agents.upsert(a);
      setEditing(false);
    } catch (e) {
      props.onError(e);
    } finally {
      setBusy(false);
    }
  };
  const swap = async () => {
    const to = target() || targets()[0];
    if (!to) return;
    setBusy(true);
    try {
      const r = await api.agents.swapSubscription(props.agent.id, to);
      setResult(appliedText(r));
      setTarget("");
      agents.upsert(await api.agents.get(props.agent.id));
    } catch (e) {
      props.onError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="stack" style={{ gap: "4px" }}>
      <Show
        when={!editing()}
        fallback={
          <>
            <PoolPicker harness={props.agent.harness} current={current()} value={draft()} onChange={setDraft} />
            <span class="row">
              <button class="small primary" disabled={busy() || samePool(normalizePool(draft(), current()), pool())} onClick={save}>
                Save
              </button>
              <button class="small" onClick={() => setEditing(false)}>
                Cancel
              </button>
            </span>
          </>
        }
      >
        <span class="row" style={{ "flex-wrap": "wrap" }}>
          <Show when={pool().length} fallback={<span class="muted">none</span>}>
            <For each={pool()}>{(n) => <span class={`chip ${n === props.agent.subscription ? "ok" : ""}`}>{n}</span>}</For>
          </Show>
          <button
            class="small"
            disabled={fileManaged() || !props.agent.subscription}
            title={fileManaged() ? "A file agent: edit its definition file." : !props.agent.subscription ? "Set a subscription first." : undefined}
            onClick={() => {
              setDraft(pool());
              setResult(null);
              setEditing(true);
            }}
          >
            Edit
          </button>
        </span>
      </Show>
      <Show when={!editing() && targets().length}>
        <span class="row" style={{ "flex-wrap": "wrap" }}>
          <span class="small">Swap to</span>
          <select aria-label="Swap to" value={target() || targets()[0]} onChange={(e) => setTarget(e.currentTarget.value)}>
            <For each={targets()}>
              {(n) => (
                <option value={n} selected={n === (target() || targets()[0])}>
                  {n}
                </option>
              )}
            </For>
          </select>
          <button class="small" disabled={busy()} onClick={swap}>
            Swap
          </button>
        </span>
      </Show>
      <Show when={result()}>
        <span class="muted small" role="status">
          {result()}
        </span>
      </Show>
    </div>
  );
}

/**
 * The agent's system prompt, and a control to change it with
 * `PATCH /api/agents/{id}` (empty sends `null`: the harness default). The
 * change applies at the next start, as for the subscription.
 */
function SystemPromptCell(props: { agent: Agent; running: boolean; onError: (e: unknown) => void }) {
  const [editing, setEditing] = createSignal(false);
  const [draft, setDraft] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [saved, setSaved] = createSignal(false);
  const fileManaged = () => props.agent.source === "file";
  const current = () => props.agent.system_prompt ?? "";
  const next = () => (draft().trim() ? draft() : null);
  const problem = () => systemPromptProblem(draft());

  const save = async () => {
    setBusy(true);
    try {
      const a = await api.agents.update(props.agent.id, { system_prompt: next() });
      agents.upsert(a);
      setEditing(false);
      setSaved(true);
    } catch (e) {
      props.onError(e);
    } finally {
      setBusy(false);
    }
  };
  const restart = async () => {
    try {
      await api.agents.restart(props.agent.id);
      setSaved(false);
    } catch (e) {
      props.onError(e);
    }
  };

  return (
    <div class="stack" style={{ gap: "4px" }}>
      <Show
        when={!editing()}
        fallback={
          <>
            <SystemPromptField value={draft()} onChange={setDraft} label="New system prompt" />
            <Show when={problem()}>{(p) => <span class="small" style={{ color: "var(--danger)" }}>{p()}</span>}</Show>
            <span class="row">
              <button class="small primary" disabled={busy() || !!problem() || (next() ?? "") === current()} onClick={save}>
                Save
              </button>
              <button class="small" onClick={() => setEditing(false)}>
                Cancel
              </button>
            </span>
          </>
        }
      >
        <span class="row" style={{ "flex-wrap": "wrap", "align-items": "flex-start" }}>
          <Show when={current()} fallback={<span class="muted">harness default</span>}>
            <pre class="mono small" style={{ margin: 0, "white-space": "pre-wrap", "max-height": "12em", overflow: "auto", flex: "1 1 20em" }}>
              {current()}
            </pre>
          </Show>
          <button
            class="small"
            disabled={fileManaged()}
            title={fileManaged() ? "A file agent: edit its definition file." : undefined}
            onClick={() => {
              setDraft(current());
              setSaved(false);
              setEditing(true);
            }}
          >
            Edit
          </button>
        </span>
      </Show>
      <Show when={editing() || saved()}>
        <span class="muted small">
          A new system prompt applies at the next start or restart.
          <Show when={saved() && props.running}>
            {" "}
            <button class="small" onClick={restart}>
              Restart now
            </button>
          </Show>
        </span>
      </Show>
    </div>
  );
}

/**
 * The agent's skills, and a control to change them with
 * `PATCH /api/agents/{id}` (`[]` clears them). The live agent keeps its
 * skills until the next start: `skills_restart` says that a restart applies
 * the list.
 */
function SkillsCell(props: { agent: Agent; running: boolean; onError: (e: unknown) => void }) {
  const [editing, setEditing] = createSignal(false);
  const [draft, setDraft] = createSignal<string[]>([]);
  const [busy, setBusy] = createSignal(false);
  const fileManaged = () => props.agent.source === "file";
  const current = () => props.agent.skills ?? [];
  const same = () => draft().length === current().length && draft().every((x, i) => x === current()[i]);

  const save = async () => {
    setBusy(true);
    try {
      agents.upsert(await api.agents.update(props.agent.id, { skills: draft() }));
      setEditing(false);
    } catch (e) {
      props.onError(e);
    } finally {
      setBusy(false);
    }
  };
  const restart = async () => {
    try {
      await api.agents.restart(props.agent.id);
    } catch (e) {
      props.onError(e);
    }
  };

  return (
    <div class="stack" style={{ gap: "4px" }}>
      <Show
        when={!editing()}
        fallback={
          <>
            <SkillsPicker value={draft()} onChange={setDraft} legend="New skills" />
            <span class="row">
              <button class="small primary" disabled={busy() || same()} onClick={save}>
                Save
              </button>
              <button class="small" onClick={() => setEditing(false)}>
                Cancel
              </button>
            </span>
            <span class="muted small">{RESTART_NOTE}</span>
          </>
        }
      >
        <span class="row" style={{ "flex-wrap": "wrap" }}>
          <Show when={current().length} fallback={<span class="muted">none</span>}>
            <span class="mono small">{skillsLabel(current())}</span>
          </Show>
          <button
            class="small"
            disabled={fileManaged()}
            title={fileManaged() ? "A file agent: edit its definition file." : undefined}
            onClick={() => {
              setDraft(current());
              setEditing(true);
            }}
          >
            Edit
          </button>
          <A class="small" href="/skills">
            Skills page
          </A>
        </span>
      </Show>
      <Show when={props.agent.skills_restart}>
        <span class="small row" role="status">
          <span class="chip warn">restart to apply</span>
          <span class="muted">The live agent runs with other skills than this list.</span>
          <Show when={props.running}>
            <button class="small" onClick={restart}>
              Restart now
            </button>
          </Show>
        </span>
      </Show>
    </div>
  );
}

/** The targets the agent holds by lease (`GET /api/agents/{id}/leases`, docs/api.md §17.7), kept live from `lease` events. */
function LeasesPanel(props: { agent: Agent; onNotice: (s: string) => void }) {
  const list = useLeaseList({
    key: () => props.agent.id,
    fetch: (id, all) => api.agents.leases(id, { all }),
    match: (_d, agentId) => agentId === props.agent.id,
    onLoaded: (id, rows) => holds.seedAgent(id, rows),
  });
  const [granting, setGranting] = createSignal(false);
  const [targets] = createResource(granting, () => api.targets.list().catch(() => []));
  return (
    <section class="card-box stack" style={{ gap: "8px" }} aria-label="Leases">
      <div class="spread">
        <h3 style={{ margin: 0 }}>Leases</h3>
        <span class="row" style={{ gap: "8px" }}>
          <label class="row small">
            <input type="checkbox" checked={list.all()} onChange={(e) => list.setAll(e.currentTarget.checked)} />
            history
          </label>
          <button class="small" onClick={() => setGranting(true)}>
            Grant a target…
          </button>
        </span>
      </div>
      <ErrorBanner error={list.error()} />
      <Show when={!list.error()}>
        <LeaseTable leases={list.list()} show="target" history={list.all()} loading={list.loading()} onChange={list.put} />
      </Show>
      <Show when={granting() && targets()}>
        {(ts) => (
          <GrantDialog
            agent={props.agent}
            targets={ts()}
            onClose={() => setGranting(false)}
            onGranted={(l) => {
              setGranting(false);
              list.put(l);
              props.onNotice(grantNotice(l));
            }}
          />
        )}
      </Show>
    </section>
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
                  .subscription!.windows.map((w) => `${w.window} ${windowValue(w)}${w.resets_at ? ` (resets in ${timeUntil(w.resets_at, now())})` : ""}`)
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
