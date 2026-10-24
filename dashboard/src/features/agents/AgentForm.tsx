// Create or edit an agent: every AgentDef field (docs/api.md §6.1).

import { createResource, createSignal, For, Show } from "solid-js";
import { ErrorBanner, Modal } from "../../components/ui";
import { GRANTABLE_ROLES, HARNESSES, RESTART_POLICIES, type Agent, type Harness, type RestartPolicy, type Role } from "../../api/types";
import { agents, api, projects, subscriptions } from "../../stores";
import { agentToForm, emptyAgentForm, formToDef, formToPatch, validateAgentForm, type AgentFormValues } from "./form";

export function AgentForm(props: { agent?: Agent; project?: string | null; onClose: () => void; onSaved: (a: Agent) => void }) {
  const editing = () => !!props.agent;
  const [v, setV] = createSignal<AgentFormValues>(props.agent ? agentToForm(props.agent) : emptyAgentForm(props.project ?? projects.selected() ?? ""));
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const set = <K extends keyof AgentFormValues>(k: K, value: AgentFormValues[K]) => setV({ ...v(), [k]: value });
  const [targets] = createResource(
    () => v().project || null,
    (p) => api.projects.targets(p).catch(() => []),
  );
  const problems = () => validateAgentForm(v());

  const submit = async (e: Event) => {
    e.preventDefault();
    if (problems().length) return setError(problems().join(" "));
    setBusy(true);
    try {
      const saved = props.agent ? await api.agents.update(props.agent.id, formToPatch(props.agent, v())) : await api.agents.create(formToDef(v()));
      agents.upsert(saved);
      props.onSaved(saved);
      props.onClose();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };

  const toggle = <T,>(list: T[], x: T, on: boolean) => (on ? [...new Set([...list, x])] : list.filter((y) => y !== x));

  return (
    <Modal title={editing() ? `Edit ${props.agent!.project}/${props.agent!.name}` : "New agent"} onClose={() => props.onClose()}>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={props.agent?.source === "file"}>
        <p class="notice small">This agent comes from a definition file. The daemon refuses changes here (file_managed). Edit the file, then reload.</p>
      </Show>
      <form class="stack" onSubmit={submit}>
        <div class="form-grid">
          <label>
            Name
            <input value={v().name} disabled={editing()} onInput={(e) => set("name", e.currentTarget.value)} required pattern="[a-z0-9][a-z0-9_\-]{0,62}" placeholder="worker" />
          </label>
          <label>
            Project
            <select value={v().project} disabled={editing()} onChange={(e) => set("project", e.currentTarget.value)} required>
              <option value="" disabled>
                Pick a project
              </option>
              <For each={projects.projects()}>
                {(p) => (
                  <option value={p.name} selected={p.name === v().project}>
                    {p.name}
                  </option>
                )}
              </For>
            </select>
          </label>
          <label>
            Harness
            <select value={v().harness} onChange={(e) => set("harness", e.currentTarget.value as Harness)}>
              <For each={HARNESSES}>{(h) => <option value={h}>{h}</option>}</For>
            </select>
          </label>
          <label>
            Subscription
            <input list="agent-subscriptions" value={v().subscription} onInput={(e) => set("subscription", e.currentTarget.value)} placeholder="anthropic-max" />
            <datalist id="agent-subscriptions">
              <For each={subscriptions.list()}>{(s) => <option value={s.name}>{s.plan}</option>}</For>
            </datalist>
          </label>
          <label>
            Model
            <input value={v().model} onInput={(e) => set("model", e.currentTarget.value)} placeholder="anthropic/claude-opus-5-5 (pi, codex)" />
          </label>
          <label>
            Prompt template
            <input value={v().prompt} onInput={(e) => set("prompt", e.currentTarget.value)} placeholder="worker (a name under prompts/)" />
          </label>
          <label>
            Restart policy
            <select value={v().restart} onChange={(e) => set("restart", e.currentTarget.value as RestartPolicy)}>
              <For each={RESTART_POLICIES}>{(r) => <option value={r}>{r}</option>}</For>
            </select>
          </label>
        </div>

        <Show
          when={!editing()}
          fallback={<p class="muted small">Change roles in the Roles panel of the agent page. An emptied subscription, model, or prompt stays as it was: the daemon cannot clear them.</p>}
        >
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
            <Show when={targets()?.length} fallback={<span class="muted small">This project has no targets.</span>}>
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

        <Show when={problems().length && v().name}>
          <ul class="small" style={{ color: "var(--danger)", margin: 0 }}>
            <For each={problems()}>{(p) => <li>{p}</li>}</For>
          </ul>
        </Show>
        <div class="row">
          <button class="primary" type="submit" disabled={busy() || problems().length > 0}>
            {editing() ? "Save" : "Create agent"}
          </button>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}
