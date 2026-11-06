// The agent fields that an agent and a template share: harness,
// subscription, model, prompt, system prompt (persona and text in one selector), skills, restart, idle stop, auto compact, roles, and policy. No name and no
// project: the agent form and the template form add those.

import { createResource, createSignal, For, Show } from "solid-js";
import { GRANTABLE_ROLES, HARNESSES, roleLabel, RESTART_POLICIES, type Harness, type RestartPolicy, type Role } from "../../api/types";
import { api, subscriptions } from "../../stores";
import { compactMaxAgeProblem, idleStopProblem, type AgentFormValues, type CompactSwitch } from "./form";
import { SystemPromptSelect } from "./SystemPromptSelect";
import { prunePool, reconcileSubscription } from "./subscription";
import { defaultModelHint, ModelField, PoolPicker, SubscriptionSelect } from "./SubscriptionFields";
import { SkillsPicker } from "../skills/SkillsPicker";

// The global idle stop (`GET /api/health` `idle_stop`). One request for each page load.
let idleStopDefault: Promise<string | undefined> | undefined;

/** The global idle stop, such as `10m` or `off`. Undefined until the health answers, or when it does not say. */
export function useGlobalIdleStop(): () => string | undefined {
  idleStopDefault ??= api
    .health()
    .then((h) => h.idle_stop)
    .catch(() => undefined);
  const [value] = createResource(() => idleStopDefault);
  return () => value();
}

// The global auto compact settings and the harnesses that can compact (`GET /api/health`). One request for each page load.
let compactDefaults: Promise<{ on_idle?: boolean; max_age?: string; harnesses?: string[] }> | undefined;

/** The global auto compact settings. Empty until the health answers, or when it does not say. */
export function useCompactDefaults(): () => { on_idle?: boolean; max_age?: string; harnesses?: string[] } {
  compactDefaults ??= api
    .health()
    .then((h) => ({ on_idle: h.compact_on_idle, max_age: h.compact_max_age, harnesses: h.compact_harnesses }))
    .catch(() => ({}));
  const [value] = createResource(() => compactDefaults);
  return () => value() ?? {};
}

export const COMPACT_HELP = "Each compaction is one model call. It runs only after a turn, and never while the agent works.";

/**
 * Auto compact: compact at the idle stop (on, off, or the default), and the
 * maximum age of the session context. A harness that cannot compact gets a
 * note: the daemon ignores the settings of its agents.
 */
export function AutoCompactFields(props: { harness: string; onIdle: CompactSwitch; maxAge: string; onChange: (onIdle: CompactSwitch, maxAge: string) => void }) {
  const d = useCompactDefaults();
  const supported = () => !d().harnesses || d().harnesses!.includes(props.harness);
  const onIdleDefault = () => (d().on_idle === undefined ? "default" : `default (${d().on_idle ? "on" : "off"})`);
  return (
    <fieldset class="stack" style={{ gap: "4px" }}>
      <legend>Auto compact</legend>
      <div class="row" style={{ gap: "12px", "flex-wrap": "wrap" }}>
        <label>
          Compact on idle
          <select value={props.onIdle} onChange={(e) => props.onChange(e.currentTarget.value as CompactSwitch, props.maxAge)}>
            <option value="" selected={props.onIdle === ""}>
              {onIdleDefault()}
            </option>
            <option value="on" selected={props.onIdle === "on"}>
              on
            </option>
            <option value="off" selected={props.onIdle === "off"}>
              off
            </option>
          </select>
        </label>
        <label>
          Compact max age
          <input value={props.maxAge} onInput={(e) => props.onChange(props.onIdle, e.currentTarget.value)} placeholder={d().max_age ? `default (${d().max_age})` : "default"} pattern="off|0|([0-9]+[dhms])+" />
        </label>
      </div>
      <Show when={compactMaxAgeProblem(props.maxAge)} fallback={<span class="muted small">{supported() ? COMPACT_HELP : `auto compact not supported for ${props.harness}: the daemon ignores these settings.`}</span>}>
        {(p) => (
          <span class="small" style={{ color: "var(--danger)" }}>
            {p()}
          </span>
        )}
      </Show>
    </fieldset>
  );
}

export const IDLE_STOP_HELP = "The time the agent stays idle before the daemon stops it. Work wakes it again. off: never stop it.";

/** The idle stop: empty is the global default, `off` never stops the agent, else a duration. */
export function IdleStopField(props: { value: string; onChange: (s: string) => void }) {
  const fallback = useGlobalIdleStop();
  return (
    <label>
      Idle stop
      <input value={props.value} onInput={(e) => props.onChange(e.currentTarget.value)} placeholder={fallback() ? `default (${fallback()})` : "default"} pattern="off|0|([0-9]+[dhms])+" />
      <Show when={idleStopProblem(props.value)} fallback={<span class="muted small">{IDLE_STOP_HELP}</span>}>
        {(p) => (
          <span class="small" style={{ color: "var(--danger)" }}>
            {p()}
          </span>
        )}
      </Show>
    </label>
  );
}

const toggle = <T,>(list: T[], x: T, on: boolean) => (on ? [...new Set([...list, x])] : list.filter((y) => y !== x));

export function AgentFields(props: {
  value: AgentFormValues;
  onChange: (v: AgentFormValues) => void;
  /** Show the roles. The agent edit form changes roles in the Roles panel. */
  roles: boolean;
  /** Under the roles when they are hidden. */
  rolesNote?: string;
}) {
  const v = () => props.value;
  const set = <K extends keyof AgentFormValues>(k: K, value: AgentFormValues[K]) => props.onChange({ ...v(), [k]: value });
  // A global template has no project: it may name the global targets only.
  const [targets] = createResource(
    () => v().project || "(global)",
    (p) => (p === "(global)" ? api.targets.list().then((ts) => ts.filter((t) => !t.project)) : api.projects.targets(p)).catch(() => []),
  );
  const [subNote, setSubNote] = createSignal<string | null>(null);
  /** A new harness keeps the subscription only when it can spend it. */
  const setHarness = (h: Harness) => {
    const r = reconcileSubscription(v().subscription, h, subscriptions.list());
    props.onChange({ ...v(), harness: h, subscription: r.subscription, subscription_pool: prunePool(v().subscription_pool, subscriptions.list(), h, r.subscription) });
    setSubNote(r.note);
  };

  return (
    <>
      <div class="form-grid">
        <label>
          Harness
          <select value={v().harness} onChange={(e) => setHarness(e.currentTarget.value as Harness)}>
            <For each={HARNESSES}>
              {(h) => (
                <option value={h} selected={h === v().harness}>
                  {h}
                </option>
              )}
            </For>
          </select>
        </label>
        <label>
          Subscription
          <SubscriptionSelect
            harness={v().harness}
            value={v().subscription}
            onChange={(name) => {
              setSubNote(null);
              props.onChange({ ...v(), subscription: name, subscription_pool: prunePool(v().subscription_pool, subscriptions.list(), v().harness, name) });
            }}
          />
          <Show when={subNote()}>
            <span class="notice small" role="status">
              {subNote()}
            </span>
          </Show>
        </label>
        <label>
          Model
          <ModelField harness={v().harness} subscription={v().subscription} value={v().model} onChange={(m) => set("model", m)} />
          <Show when={!v().model.trim() && defaultModelHint(v().harness, v().subscription)}>
            {(hint) => <span class="muted small">{hint()}</span>}
          </Show>
        </label>
        <label>
          Prompt template
          <input value={v().prompt} onInput={(e) => set("prompt", e.currentTarget.value)} placeholder="worker (a name under prompts/)" />
        </label>
        <label>
          Restart policy
          <select value={v().restart} onChange={(e) => set("restart", e.currentTarget.value as RestartPolicy)}>
            <For each={RESTART_POLICIES}>
              {(r) => (
                <option value={r} selected={r === v().restart}>
                  {r}
                </option>
              )}
            </For>
          </select>
        </label>
        <IdleStopField value={v().idle_stop} onChange={(s) => set("idle_stop", s)} />
      </div>

      <AutoCompactFields harness={v().harness} onIdle={v().compact_on_idle} maxAge={v().compact_max_age} onChange={(onIdle, maxAge) => props.onChange({ ...v(), compact_on_idle: onIdle, compact_max_age: maxAge })} />

      <fieldset class="stack" style={{ gap: "4px" }}>
        <legend>Subscription pool</legend>
        <PoolPicker harness={v().harness} current={v().subscription} value={v().subscription_pool} onChange={(p) => set("subscription_pool", p)} />
        <span class="muted small">A swap moves the agent to another account of the pool. Same provider only.</span>
      </fieldset>

      <SystemPromptSelect value={{ persona: v().persona, system_prompt: v().system_prompt }} roles={v().roles} onChange={(f) => props.onChange({ ...v(), ...f })} />
      <SkillsPicker value={{ skills: v().skills, skills_off: v().skills_off, default_skills: v().default_skills }} onChange={(s) => props.onChange({ ...v(), ...s })} />

      <Show when={props.roles} fallback={<Show when={props.rolesNote}>{(n) => <p class="muted small">{n()}</p>}</Show>}>
        <fieldset class="row">
          <legend>Roles (above member)</legend>
          <For each={GRANTABLE_ROLES}>
            {(r) => (
              <label class="row small">
                <input type="checkbox" checked={v().roles.includes(r)} onChange={(e) => set("roles", toggle<Role>(v().roles, r, e.currentTarget.checked))} /> {roleLabel(r)}
              </label>
            )}
          </For>
        </fieldset>
      </Show>

      <fieldset class="stack" style={{ gap: "8px" }}>
        <legend>Policy</legend>
        <div>
          <div class="small">Targets</div>
          <Show when={targets()?.length} fallback={<span class="muted small">{v().project ? "This project has no targets." : "There are no global targets."}</span>}>
            <div class="row" style={{ "flex-wrap": "wrap" }}>
              <For each={targets()}>
                {(t) => (
                  <label class="row small">
                    <input type="checkbox" checked={v().targets.includes(t.name)} onChange={(e) => set("targets", toggle(v().targets, t.name, e.currentTarget.checked))} /> {t.name}
                    <span class="muted">({t.scope ?? (t.project ? "project" : "global")})</span>
                  </label>
                )}
              </For>
            </div>
          </Show>
        </div>
        <label>
          Mail to (agents of the project; * means all)
          <input value={v().mail_to} onInput={(e) => set("mail_to", e.currentTarget.value)} placeholder="*" />
        </label>
        <label>
          Cross-project (project/agent, comma separated)
          <input value={v().cross_project} onInput={(e) => set("cross_project", e.currentTarget.value)} placeholder="ops/triage" />
        </label>
      </fieldset>
    </>
  );
}
