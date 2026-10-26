// Create or edit an agent: every AgentDef field (docs/api.md §6.1). A new
// agent can start from a template (§21): the pick fills the form, and the
// user can change any field before the create.

import { createResource, createSignal, For, Show } from "solid-js";
import { ErrorBanner, Modal } from "../../components/ui";
import type { Agent } from "../../api/types";
import { agents, api, projects } from "../../stores";
import { agentToForm, emptyAgentForm, formToPatch, validateAgentForm, type AgentFormValues } from "./form";
import { AgentFields } from "./AgentFields";
import { formToCreate, resolvedToForm, scopeLabel, selectable, useKey } from "../templates/model";

export function AgentForm(props: { agent?: Agent; project?: string | null; onClose: () => void; onSaved: (a: Agent) => void }) {
  const editing = () => !!props.agent;
  const [v, setV] = createSignal<AgentFormValues>(props.agent ? agentToForm(props.agent) : emptyAgentForm(props.project ?? projects.selected() ?? ""));
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const set = <K extends keyof AgentFormValues>(k: K, value: AgentFormValues[K]) => setV({ ...v(), [k]: value });
  const problems = () => validateAgentForm(v());

  // The template key of the create, or "" for none.
  const [template, setTemplate] = createSignal("");
  const [templates] = createResource(
    () => (editing() ? null : v().project || null),
    (p) =>
      api.templates
        .list({ project: p })
        .then((list) => selectable(list, p))
        .catch(() => []),
  );
  const pickTemplate = async (key: string) => {
    setTemplate(key);
    if (!key) return;
    try {
      setV(resolvedToForm(await api.templates.resolve(key, v().project), v().name));
      setError(null);
    } catch (err) {
      setTemplate("");
      setError(err);
    }
  };
  const setProject = (p: string) => {
    // A template belongs to a project or is global: pick it again for the new project.
    setTemplate("");
    set("project", p);
  };

  const submit = async (e: Event) => {
    e.preventDefault();
    if (problems().length) return setError(problems().join(" "));
    setBusy(true);
    try {
      const saved = props.agent ? await api.agents.update(props.agent.id, formToPatch(props.agent, v())) : await api.agents.create(formToCreate(v(), template() || null));
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
            <select value={v().project} disabled={editing()} onChange={(e) => setProject(e.currentTarget.value)} required>
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
          <Show when={!editing()}>
            <label>
              Template
              <select value={template()} onChange={(e) => void pickTemplate(e.currentTarget.value)} disabled={!v().project} aria-label="Template">
                <option value="">None: set every field</option>
                <For each={templates()}>
                  {(t) => (
                    <option value={useKey(t)} selected={useKey(t) === template()}>
                      {t.name} ({scopeLabel(t)})
                    </option>
                  )}
                </For>
              </select>
              <Show when={template()}>
                <span class="muted small">The template filled the fields. Change any of them; the values are copied, with no live link.</span>
              </Show>
            </label>
          </Show>
        </div>

        <AgentFields
          value={v()}
          onChange={setV}
          roles={!editing()}
          rolesNote="Change roles in the Roles panel of the agent page. An emptied model or prompt is cleared. A new subscription, model, or system prompt applies at the next start."
        />

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
