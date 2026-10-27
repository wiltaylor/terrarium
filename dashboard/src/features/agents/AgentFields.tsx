// The agent fields that an agent and a template share: harness,
// subscription, model, prompt, system prompt, persona, restart, roles, and policy. No name and no
// project: the agent form and the template form add those.

import { createResource, createSignal, createUniqueId, For, Show } from "solid-js";
import { GRANTABLE_ROLES, HARNESSES, RESTART_POLICIES, type Harness, type RestartPolicy, type Role } from "../../api/types";
import { api, subscriptions } from "../../stores";
import { PERSONAS, personaProblem, type AgentFormValues } from "./form";
import { prunePool, reconcileSubscription } from "./subscription";
import { defaultModelHint, ModelField, PoolPicker, SubscriptionSelect } from "./SubscriptionFields";

export const SYSTEM_PROMPT_HELP = "Replaces the harness's default system prompt. Terrarium's protocol instructions are always added after it.";

/** The system prompt textarea. The text is user content: it stays a textarea value. */
export function SystemPromptField(props: { value: string; onChange: (s: string) => void; label?: string }) {
  return (
    <label>
      {props.label ?? "System prompt"}
      <textarea class="mono" rows={5} value={props.value} onInput={(e) => props.onChange(e.currentTarget.value)} placeholder="Empty: the harness's default system prompt" />
      <span class="muted small">{SYSTEM_PROMPT_HELP}</span>
    </label>
  );
}

export const PERSONA_HELP = "Empty: by role (leader for project_admin or sysadmin, else worker). The persona replaces the harness's default system prompt. A system prompt wins over it. none keeps the harness default.";

/** The persona: a text input with the known names. Empty is "by role". */
export function PersonaField(props: { value: string; onChange: (s: string) => void }) {
  const list = createUniqueId();
  return (
    <label>
      Persona
      <input value={props.value} list={list} onInput={(e) => props.onChange(e.currentTarget.value)} placeholder="by role" pattern="[A-Za-z0-9_\-]{0,64}" />
      <datalist id={list}>
        <For each={PERSONAS}>{(p) => <option value={p} />}</For>
      </datalist>
      <Show when={personaProblem(props.value)} fallback={<span class="muted small">{PERSONA_HELP}</span>}>
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
      </div>

      <fieldset class="stack" style={{ gap: "4px" }}>
        <legend>Subscription pool</legend>
        <PoolPicker harness={v().harness} current={v().subscription} value={v().subscription_pool} onChange={(p) => set("subscription_pool", p)} />
        <span class="muted small">A swap moves the agent to another account of the pool. Same provider only.</span>
      </fieldset>

      <SystemPromptField value={v().system_prompt} onChange={(s) => set("system_prompt", s)} />
      <PersonaField value={v().persona} onChange={(s) => set("persona", s)} />

      <Show when={props.roles} fallback={<Show when={props.rolesNote}>{(n) => <p class="muted small">{n()}</p>}</Show>}>
        <fieldset class="row">
          <legend>Roles (above member)</legend>
          <For each={GRANTABLE_ROLES}>
            {(r) => (
              <label class="row small">
                <input type="checkbox" checked={v().roles.includes(r)} onChange={(e) => set("roles", toggle<Role>(v().roles, r, e.currentTarget.checked))} /> {r}
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
