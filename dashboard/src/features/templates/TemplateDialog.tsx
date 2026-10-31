// Create or edit an agent template (docs/api.md §21): the template name, its
// scope (a project, or global), a description, and the agent fields of the
// agent form. The scope does not change after the create. A save writes the
// template's WCL file in the config repo, without the comments of a file
// edited by hand.

import { createSignal, For, Show } from "solid-js";
import { ErrorBanner, Modal } from "../../components/ui";
import type { AgentTemplate } from "../../api/types";
import { api, projects } from "../../stores";
import { AgentFields } from "../agents/AgentFields";
import { emptyTemplateForm, formToNewTemplate, formToTemplatePatch, pathKey, templateToForm, validateTemplateForm, type TemplateFormValues } from "./model";

export function TemplateDialog(props: { template?: AgentTemplate; scope?: string; onClose: () => void; onSaved: (t: AgentTemplate) => void }) {
  const editing = () => !!props.template;
  const [v, setV] = createSignal<TemplateFormValues>(props.template ? templateToForm(props.template) : emptyTemplateForm(props.scope ?? ""));
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const problems = () => validateTemplateForm(v());
  // The agent fields see the scope as their project, for the target list.
  const setScope = (scope: string) => setV({ ...v(), scope, agent: { ...v().agent, project: scope, targets: [] } });

  const submit = async (e: Event) => {
    e.preventDefault();
    if (problems().length) return setError(problems().join(" "));
    setBusy(true);
    try {
      const saved = props.template ? await api.templates.update(props.template.id, formToTemplatePatch(props.template, v())) : await api.templates.create(formToNewTemplate(v()));
      props.onSaved(saved);
      props.onClose();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal title={editing() ? `Edit template ${pathKey(props.template!)}` : "New agent template"} onClose={() => props.onClose()}>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <form class="stack" onSubmit={submit}>
        <div class="form-grid">
          <label>
            Name
            <input value={v().name} onInput={(e) => setV({ ...v(), name: e.currentTarget.value })} required pattern="[a-z0-9][a-z0-9_\-]{0,62}" placeholder="worker" />
          </label>
          <label>
            Scope
            <select value={v().scope} disabled={editing()} onChange={(e) => setScope(e.currentTarget.value)}>
              <option value="" selected={v().scope === ""}>
                Global (every project)
              </option>
              <For each={projects.projects()}>
                {(p) => (
                  <option value={p.name} selected={p.name === v().scope}>
                    {p.name}
                  </option>
                )}
              </For>
            </select>
          </label>
          <label style={{ "grid-column": "1 / -1" }}>
            Description
            <input value={v().description} onInput={(e) => setV({ ...v(), description: e.currentTarget.value })} placeholder="What an agent of this template does" />
          </label>
        </div>
        <p class="muted small">An agent made from this template gets a copy of these values. A later change to the template does not change the agents made before.</p>

        <AgentFields value={v().agent} onChange={(agent) => setV({ ...v(), agent })} roles />

        <Show when={problems().length && v().name}>
          <ul class="small" style={{ color: "var(--danger)", margin: 0 }}>
            <For each={problems()}>{(p) => <li>{p}</li>}</For>
          </ul>
        </Show>
        <div class="row">
          <button class="primary" type="submit" disabled={busy() || problems().length > 0}>
            {editing() ? "Save" : "Create template"}
          </button>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}

/** "Save as template" on an agent page: a template from the agent's settings now. */
export function SaveAsTemplateDialog(props: { agent: { id: string; name: string; project: string }; onClose: () => void; onSaved: (t: AgentTemplate) => void }) {
  const [name, setName] = createSignal(props.agent.name);
  const [scope, setScope] = createSignal(props.agent.project);
  const [description, setDescription] = createSignal("");
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const submit = async (e: Event) => {
    e.preventDefault();
    setBusy(true);
    try {
      const t = await api.templates.fromAgent(props.agent.id, { name: name().trim(), project: scope() || null, description: description().trim() });
      props.onSaved(t);
      props.onClose();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal title={`Save ${props.agent.project}/${props.agent.name} as a template`} onClose={() => props.onClose()}>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <form class="stack" onSubmit={submit}>
        <label>
          Template name
          <input value={name()} onInput={(e) => setName(e.currentTarget.value)} required pattern="[a-z0-9][a-z0-9_\-]{0,62}" />
        </label>
        <label>
          Scope
          <select value={scope()} onChange={(e) => setScope(e.currentTarget.value)}>
            <option value={props.agent.project}>{props.agent.project} (this project)</option>
            <option value="">Global (needs sysadmin)</option>
          </select>
        </label>
        <label>
          Description
          <input value={description()} onInput={(e) => setDescription(e.currentTarget.value)} />
        </label>
        <p class="muted small">The template copies the harness, subscription, model, prompt, system prompt, roles, policy, and restart policy of the agent.</p>
        <div class="row">
          <button class="primary" type="submit" disabled={busy()}>
            Save template
          </button>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}
