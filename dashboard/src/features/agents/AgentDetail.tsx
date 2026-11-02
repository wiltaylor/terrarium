import { A, useNavigate, useParams, useSearchParams } from "@solidjs/router";
import { createResource, createSignal, For, Match, onCleanup, onMount, Show, Switch } from "solid-js";
import { GrantDialog, grantNotice, LeaseChips, LeaseTable, useLeaseList } from "../../components/Leases";
import { HoldBadge } from "../../components/HoldBadge";
import { SelfReport } from "../../components/SelfReport";
import { EventHistory } from "../../components/EventHistory";
import { StatusBadge } from "../../components/StatusBadge";
import { Terminal } from "../../components/Terminal";
import { ConfirmButton, ErrorBanner, PageHead, Tabs, useNow } from "../../components/ui";
import { GRANTABLE_ROLES, type Agent, type AgentStatus, type ResetKind, type ResetResult, type Role } from "../../api/types";
import { ago, compact, orderWindows, subscriptionTitle, timeUntil, windowLabel, windowValue } from "../../lib/format";
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
import { useCompactDefaults, useGlobalIdleStop } from "./AgentFields";
import { SystemPromptSelect, usePromptLibrary } from "./SystemPromptSelect";
import { choiceLabel, choiceOf, missingPersonaWarning, type PromptFields } from "./systemPrompt";
import { autoCompactText, idleStopText, systemPromptProblem } from "./form";
import { WaitsPanel, WatchesPanel } from "./Waits";
import { QuestionsPanel } from "../inbox/QuestionsPanel";
import { SkillsPicker } from "../skills/SkillsPicker";
import { RESTART_NOTE, skillsLabel, type SkillSelection } from "../skills/model";
import { compactButton, lastCompactedText, lifecycleActions, newSessionButton, resetChip, resetNotice, type LifecycleAction } from "../../lib/lifecycle";
import { ResetDialog } from "./ResetDialog";
import { EphemeralLine, SpawnedPanel } from "./Spawned";
import { spawnedTabLabel, spawnRightsText } from "./spawn";

/** The API call of a lifecycle button. Resume clears the pause (`unpause`). */
const lifecycleCall = (kind: LifecycleAction["kind"], id: string) =>
  kind === "pause" ? api.agents.pause(id) : kind === "resume" ? api.agents.unpause(id) : api.agents.restart(id);

/**
 * The page of an agent that the daemon removed. The back link opens the agents
 * of the project of the agent, or the project cards if the project is not known.
 */
function RemovedAgent(props: { project?: string }) {
  return (
    <div class="stack">
      <PageHead title="Agent removed" />
      <p class="muted">
        This agent was removed. <A href={props.project ? `/p/${props.project}/agents` : "/projects"}>Back to the agents</A>
      </p>
    </div>
  );
}

/** Statuses with a live session: the PTY can attach. */
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
  // The open reset dialog: Compact or New session.
  const [resetting, setResetting] = createSignal<ResetKind | null>(null);
  const [fetched] = createResource(id, (key) => api.agents.get(key).catch((e: unknown) => (setError(e), null)));
  const agent = (): Agent | undefined => agents.byId(id()) ?? agents.byId(fetched()?.id ?? "") ?? fetched() ?? undefined;
  const running = () => RUNNING.includes(agent()?.status ?? "stopped");
  const globalIdleStop = useGlobalIdleStop();
  const compactDefaults = useCompactDefaults();
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
      const r = await fn();
      setError(null);
      // 202: the body shows the old status. The events stream brings the change.
      const queued = !!r && typeof r === "object" && "queued" in r && r.queued === true;
      setNotice(queued ? `${label} queued. ${agent()?.paused ? "The agent is paused: it runs the turn after you resume it." : "The agent wakes to run it."}` : `${label} requested. The status follows from the event stream.`);
      setTimeout(() => setNotice(null), 4000);
      // The pause flag and the queued count do not come in status events: read the list again.
      agents.refreshSoon(300);
    } catch (e) {
      setError(e);
    }
  };

  /** The answer of a reset request: the notice, and `reset_pending` at once (the events bring the rest). */
  const resetDone = (r: ResetResult) => {
    const { reset, ...view } = r;
    setError(null);
    agents.upsert(view);
    setNotice(resetNotice(reset, !!view.paused));
    setTimeout(() => setNotice(null), 6000);
    agents.refreshSoon(300);
  };

  /** Cancel an ephemeral agent: the daemon closes it; the page then shows it removed. */
  const cancelEphemeral = async (a: Agent) => {
    try {
      await api.agents.cancel(a.id);
      setRemovedKey(id());
    } catch (e) {
      setError(e);
    }
  };

  const remove = async (a: Agent) => {
    try {
      await api.agents.remove(a.id, true);
      agents.drop(a.id);
      navigate(`/p/${a.project}/agents`);
    } catch (e) {
      setError(e);
    }
  };

  return (
    <Show
      when={!removed() && agent()}
      fallback={
        <Show when={!removed()} fallback={<RemovedAgent project={agent()?.project} />}>
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
                {a().name} <StatusBadge agent={a()} /> <HoldBadge agentId={a().id} long /> <LeaseChips agentId={a().id} />
                <Show when={resetChip(a(), ago)}>
                  {(chip) => (
                    <span class="chip warn" title={chip().title} aria-label={`${chip().label}. ${chip().title}`}>
                      {chip().label}
                    </span>
                  )}
                </Show>
              </span>
            }
          >
            {/* No Start and no Stop: work wakes the agent, and the idle timeout stops it. */}
            {/* An ephemeral agent has no pause: Cancel closes it (docs/api.md §6.10). */}
            <For each={lifecycleActions(a()).filter((b) => !a().ephemeral || b.kind === "restart")}>
              {(b) => (
                <button onClick={() => act(b.label, () => lifecycleCall(b.kind, a().id))} disabled={b.disabled} title={b.title}>
                  {b.label}
                </button>
              )}
            </For>
            {(() => {
              const c = () => compactButton(a(), quota.holdsFor(a().id).length > 0);
              const n = () => newSessionButton(a());
              return (
                <>
                  <button onClick={() => setResetting("compact")} disabled={c().disabled} title={c().title}>
                    Compact
                  </button>
                  <button onClick={() => setResetting("new_session")} disabled={n().disabled} title={n().title}>
                    New session
                  </button>
                </>
              );
            })()}
            <button onClick={() => setEditing(true)}>Edit</button>
            <button onClick={() => setSaving(true)}>Save as template</button>
            <Show
              when={a().ephemeral}
              fallback={<ConfirmButton label="Delete" confirmLabel={running() ? "Yes, stop and delete" : "Yes, delete"} onConfirm={() => remove(a())} />}
            >
              <ConfirmButton label="Cancel" confirmLabel="Yes, cancel the agent" onConfirm={() => cancelEphemeral(a())} />
            </Show>
          </PageHead>
          <Show when={a().ephemeral}>
            <EphemeralLine agent={a()} />
          </Show>
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
              <dt>Skills</dt>
              <dd>
                <SkillsCell agent={a()} running={running()} onError={setError} />
              </dd>
              <dt>Restart</dt>
              <dd>{a().restart ?? "on-failure"}</dd>
              <dt>Spawn rights</dt>
              <dd>
                <span classList={{ muted: !a().policy?.spawn }}>{spawnRightsText(a().policy?.spawn)}</span>
              </dd>
              <dt>Idle stop</dt>
              <dd>
                {idleStopText(a().idle_stop, globalIdleStop())}
                <Show when={a().status_reason && a().status === "stopped"}>
                  <span class="muted small"> · last stop: {a().status_reason}</span>
                </Show>
              </dd>
              <dt>Auto compact</dt>
              <dd>
                <span classList={{ muted: a().compact_supported === false }}>{autoCompactText(a(), compactDefaults())}</span>
                <Show when={lastCompactedText(a(), (t) => ago(t))}>
                  {(t) => (
                    <span class="muted small" style={a().last_compact_ok === false ? { color: "var(--warn)" } : undefined}>
                      {" "}
                      · {t()}
                    </span>
                  )}
                </Show>
              </dd>
              <Show when={a().template}>
                {(t) => (
                  <>
                    <dt>Template</dt>
                    <dd>
                      <A href={t().project ? `/p/${encodeURIComponent(t().project!)}/settings/templates` : "/settings/templates"}>{t().name}</A> <span class="chip">{t().project ?? "global"}</span> <span class="muted small">revision {t().revision} at the create; a copy, not a live link</span>
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
              <input
                style={{ flex: 1 }}
                placeholder={a().paused ? "The agent is paused: a turn waits until you resume it" : running() ? "Type a turn for this agent…" : "Type a turn: the agent wakes to run it"}
                value={turn()}
                onInput={(e) => setTurn(e.currentTarget.value)}
                aria-label="Turn text"
              />
              <button class="primary" type="submit">
                Send turn
              </button>
            </form>
          </Show>

          <div>
            <Tabs tabs={AGENT_TABS.map((t) => (t.id === "spawned" ? { ...t, label: spawnedTabLabel(a().spawned_running) } : t))} active={tab()} onChange={setTab} />
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
              <Match when={tab() === "spawned"}>
                <SpawnedPanel owner={a()} />
              </Match>
            </Switch>
          </div>

          <Show when={editing()}>
            <AgentForm agent={a()} onClose={() => setEditing(false)} onSaved={() => setNotice("Saved.")} />
          </Show>
          <Show when={resetting()}>
            {(kind) => <ResetDialog agent={a()} kind={kind()} title={kind() === "compact" ? "Compact" : "New session"} onClose={() => setResetting(null)} onDone={resetDone} />}
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
      return s ? { name: s.name, display_name: s.display_name, provider: s.provider, plan: s.plan, login_status: s.login_status, harnesses: s.harnesses, default_model: s.default_models?.[props.agent.harness] } : undefined;
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
                <A href="/settings/subscriptions">{detail()?.display_name ?? name()}</A>
                <Show when={detail()} fallback={<Show when={!detail.loading}><span class="chip warn">not found</span></Show>}>
                  {(d) => (
                    <>
                      <span class="muted small">
                        <Show when={d().display_name} fallback={<>{d().provider}{d().plan ? ` · ${d().plan}` : ""}</>}>
                          <span class="mono" title="The subscription name">{name()}</span>
                        </Show>
                      </span>
                      <span class={`chip ${d().login_status === "expired" || d().login_status === "missing" ? "warn" : "ok"}`}>{loginText(d().login_status)}</span>
                      <Show when={!d().harnesses.includes(props.agent.harness)}>
                        <span class="chip warn">not for {props.agent.harness}</span>
                      </Show>
                      <Show when={subscriptions.byName(name())?.enabled === false}>
                        <span class="chip warn" title="The subscription is disabled: the agent does not start or wake. If it runs now, it goes on until it stops.">
                          disabled
                        </span>
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
 * The agent's system prompt: by role, the harness default, an entry of the
 * system prompt library (`persona`), or its own text (`system_prompt`). Edit
 * opens the selector (SystemPromptSelect.tsx) and saves the two fields with
 * `PATCH /api/agents/{id}`. The change applies at the next start, as for the
 * subscription.
 */
function SystemPromptCell(props: { agent: Agent; running: boolean; onError: (e: unknown) => void }) {
  const [editing, setEditing] = createSignal(false);
  const [draft, setDraft] = createSignal<PromptFields>({ persona: "", system_prompt: "" });
  const [busy, setBusy] = createSignal(false);
  const [saved, setSaved] = createSignal(false);
  const library = usePromptLibrary();
  const fileManaged = () => props.agent.source === "file";
  const current = (): PromptFields => ({ persona: props.agent.persona ?? "", system_prompt: props.agent.system_prompt ?? "" });
  const choice = () => choiceOf(current());
  const roles = () => props.agent.roles.filter((r) => r !== "member");
  const missing = () => missingPersonaWarning(current().system_prompt.trim() ? "" : current().persona, library()?.map((e) => e.name));
  const problem = () => systemPromptProblem(draft().system_prompt);
  /** The fields that changed: `null` clears one. */
  const patch = () => {
    const out: { persona?: string | null; system_prompt?: string | null } = {};
    const d = draft();
    const c = current();
    if (d.persona.trim() !== c.persona.trim()) out.persona = d.persona.trim() || null;
    if ((d.system_prompt.trim() ? d.system_prompt : "") !== (c.system_prompt.trim() ? c.system_prompt : "")) out.system_prompt = d.system_prompt.trim() ? d.system_prompt : null;
    return out;
  };

  const save = async () => {
    setBusy(true);
    try {
      const a = await api.agents.update(props.agent.id, patch());
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
            <SystemPromptSelect value={draft()} roles={roles()} onChange={setDraft} />
            <span class="row">
              <button class="small primary" disabled={busy() || !!problem() || Object.keys(patch()).length === 0} onClick={save}>
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
          <span>{choiceLabel(choice(), roles())}</span>
          <Show when={choice().kind === "custom"}>
            <pre class="mono small" style={{ margin: 0, "white-space": "pre-wrap", "max-height": "12em", overflow: "auto", flex: "1 1 20em" }}>
              {current().system_prompt}
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
        <Show when={missing()}>
          {(m) => (
            <span class="notice small" role="alert">
              {m()}
            </span>
          )}
        </Show>
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
 * The agent's effective skills (the default-on skills and its `skills`,
 * less `skills_off`), and a control to change the settings with
 * `PATCH /api/agents/{id}`. The live agent keeps its skills until the next
 * start: `skills_restart` says that a restart applies them.
 */
function SkillsCell(props: { agent: Agent; running: boolean; onError: (e: unknown) => void }) {
  const [editing, setEditing] = createSignal(false);
  const [draft, setDraft] = createSignal<SkillSelection>({ skills: [], skills_off: [], default_skills: true });
  const [busy, setBusy] = createSignal(false);
  const fileManaged = () => props.agent.source === "file";
  const current = (): SkillSelection => ({ skills: props.agent.skills ?? [], skills_off: props.agent.skills_off ?? [], default_skills: props.agent.default_skills ?? true });
  const sameList = (a: string[], b: string[]) => a.length === b.length && a.every((x, i) => x === b[i]);
  const same = () => sameList(draft().skills, current().skills) && sameList(draft().skills_off, current().skills_off) && draft().default_skills === current().default_skills;

  const save = async () => {
    setBusy(true);
    try {
      agents.upsert(await api.agents.update(props.agent.id, { ...draft() }));
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
        <span class="row" style={{ "flex-wrap": "wrap" }} data-testid="agent-effective-skills">
          <Show
            when={props.agent.effective_skills}
            fallback={
              <Show when={current().skills.length} fallback={<span class="muted">none</span>}>
                <span class="mono small">{skillsLabel(current().skills)}</span>
              </Show>
            }
          >
            {(eff) => (
              <Show when={eff().length} fallback={<span class="muted">none</span>}>
                <For each={eff()}>
                  {(e) => (
                    <span class="row" style={{ gap: "2px" }}>
                      <span class="mono small">{e.name}</span>
                      <Show when={e.source === "default"}>
                        <span class="chip" title="A default skill: the agent gets it because it is default on.">
                          default
                        </span>
                      </Show>
                    </span>
                  )}
                </For>
              </Show>
            )}
          </Show>
          <Show when={!current().default_skills}>
            <span class="chip warn" title="default_skills is off: the agent gets no default skill.">
              no defaults
            </span>
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
          <A class="small" href="/settings/skills">
            Skills in Settings
          </A>
        </span>
      </Show>
      <Show when={props.agent.skills_restart}>
        <span class="small row" role="status">
          <span class="chip warn">restart to apply</span>
          <span class="muted">The live agent runs with other skills than its effective skills now.</span>
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
              {m().subscription ? subscriptionTitle(m().subscription!) : props.agent.subscription}
              <Show when={m().subscription?.display_name}>
                {" "}
                <span class="muted small mono">{m().subscription?.name}</span>
              </Show>
              <Show when={m().subscription?.local_estimate_only}> · local estimate only</Show>
            </dd>
            <dt>Windows</dt>
            <dd class="small">
              <Show when={m().subscription?.windows.length} fallback={<span class="muted">no samples</span>}>
                {orderWindows(m().subscription!.windows)
                  .map((w) => `${windowLabel(w.window)} ${windowValue(w)}${w.resets_at ? ` (resets in ${timeUntil(w.resets_at, now())})` : ""}`)
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
