import { useInstance } from "../../instance";
// Edit an agent: every AgentDef field (docs/api.md §6.1). The create of a
// new agent is NewAgentDialog.tsx; it uses the same fields.

import { createSignal, For, Show } from "solid-js";
import { ErrorBanner, Modal } from "../../components/ui";
import type { Agent } from "../../api/types";

import { agentToForm, formToPatch, validateAgentForm, type AgentFormValues } from "./form";
import { AgentFields } from "./AgentFields";
import { SpawnRightsFields } from "./Spawned";

const ROLES_NOTE = "Change roles in the Roles panel of the agent page. An emptied model or prompt is cleared. A new subscription, model, system prompt, or skill list applies at the next start.";

/** The full agent form: name, project, the agent fields, and the problems. */
export function AgentFormFields(props: {
  value: AgentFormValues;
  onChange: (v: AgentFormValues) => void;
  editing: boolean;
  nameRef?: (el: HTMLInputElement) => void;
}) {
  const { projects } = useInstance();
  const v = () => props.value;
  const set = <K extends keyof AgentFormValues>(k: K, value: AgentFormValues[K]) => props.onChange({ ...v(), [k]: value });
  const problems = () => validateAgentForm(v());
  return (
    <>
      <div class="form-grid">
        <label>
          Name
          <input ref={props.nameRef} value={v().name} disabled={props.editing} onInput={(e) => set("name", e.currentTarget.value)} required pattern="[a-z0-9][a-z0-9_\-]{0,62}" placeholder="worker" />
        </label>
        <label>
          Project
          <select value={v().project} disabled={props.editing} onChange={(e) => set("project", e.currentTarget.value)} required>
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
      </div>

      <AgentFields value={v()} onChange={props.onChange} roles={!props.editing} rolesNote={ROLES_NOTE} />

      <Show when={problems().length && v().name}>
        <ul class="small" style={{ color: "var(--danger)", margin: 0 }}>
          <For each={problems()}>{(p) => <li>{p}</li>}</For>
        </ul>
      </Show>
    </>
  );
}

export function AgentForm(props: { agent: Agent; onClose: () => void; onSaved: (a: Agent) => void }) {
  const { api, agents } = useInstance();
  const [v, setV] = createSignal<AgentFormValues>(agentToForm(props.agent));
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const problems = () => validateAgentForm(v());

  const submit = async (e: Event) => {
    e.preventDefault();
    if (problems().length) return setError(problems().join(" "));
    setBusy(true);
    try {
      const saved = await api.agents.update(props.agent.id, formToPatch(props.agent, v()));
      agents.upsert(saved);
      props.onSaved(saved);
      props.onClose();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal title={`Edit ${props.agent.project}/${props.agent.name}`} onClose={() => props.onClose()}>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={props.agent.source === "file"}>
        <p class="notice small">This agent comes from a definition file. The daemon refuses changes here (file_managed). Edit the file, then reload.</p>
      </Show>
      <form class="stack" onSubmit={submit}>
        <AgentFormFields value={v()} onChange={setV} editing />
        <SpawnRightsFields project={v().project} value={v().spawn} onChange={(sp) => setV({ ...v(), spawn: sp })} />
        <div class="row">
          <button class="primary" type="submit" disabled={busy() || problems().length > 0}>
            Save
          </button>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}
