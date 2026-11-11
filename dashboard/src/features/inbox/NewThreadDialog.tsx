import { useInstance } from "../../instance";
// "New thread" (docs/api.md §6.13): the operator opens an Inbox thread with
// a template, a scope, a topic, and a first message. Until the daemon spawns
// the thread agent from the template (TER-92), the operator may attach an
// existing agent of the scope.

import { createResource, createSignal, For, Show } from "solid-js";
import type { Thread } from "../../api/types";
import { ErrorBanner, Modal } from "../../components/ui";

import { newThreadBody, THREAD_TOPIC_MAX } from "./threads";

export function NewThreadDialog(props: { scope?: string; onClose: () => void; onOpened: (t: Thread) => void }) {
  const { api, agents, projects } = useInstance();
  const [template, setTemplate] = createSignal("");
  const [scope, setScope] = createSignal(props.scope || "global");
  const [topic, setTopic] = createSignal("");
  const [text, setText] = createSignal("");
  const [agent, setAgent] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  // The templates of the scope: a project's templates and the global ones, or the global ones only.
  const [templates] = createResource(scope, (s) => api.templates.list(s === "global" ? { scope: "global" } : { project: s }).catch(() => []));
  const templateNames = () => [...new Set((templates() ?? []).map((t) => t.name))].sort();
  const agentChoices = () =>
    agents.state.agents
      .filter((a) => !a.ephemeral && (scope() === "global" || a.project === scope()))
      .map((a) => `${a.project}/${a.name}`)
      .sort();
  const pickScope = (s: string) => {
    setScope(s);
    // The template list and the agents change with the scope.
    setTemplate("");
    if (agent() && !agentChoices().includes(agent())) setAgent("");
  };
  const submit = async (e: Event) => {
    e.preventDefault();
    const b = newThreadBody({ template: template(), scope: scope(), topic: topic(), text: text(), agent: agent() });
    if ("error" in b) return setError(b.error);
    setBusy(true);
    try {
      props.onOpened(await api.threads.open(b.body));
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal title="New thread" onClose={() => props.onClose()}>
      <form class="stack" style={{ gap: "8px" }} onSubmit={submit} aria-label="New thread">
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <label class="stack small" style={{ gap: "2px" }}>
          Scope
          <select value={scope()} onChange={(e) => pickScope(e.currentTarget.value)} aria-label="Scope">
            <option value="global" selected={scope() === "global"}>
              global (reports to meta/lead)
            </option>
            <For each={projects.projects()}>
              {(p) => (
                <option value={p.name} selected={p.name === scope()}>
                  {p.name} (reports to its lead)
                </option>
              )}
            </For>
          </select>
        </label>
        <label class="stack small" style={{ gap: "2px" }}>
          Template
          <select value={template()} onChange={(e) => setTemplate(e.currentTarget.value)} aria-label="Template">
            <option value="">No template</option>
            <For each={templateNames()}>
              {(n) => (
                <option value={n} selected={n === template()}>
                  {n}
                </option>
              )}
            </For>
          </select>
        </label>
        <label class="stack small" style={{ gap: "2px" }}>
          Agent (optional)
          <select value={agent()} onChange={(e) => setAgent(e.currentTarget.value)} aria-label="Agent">
            <option value="">No agent yet</option>
            <For each={agentChoices()}>
              {(n) => (
                <option value={n} selected={n === agent()}>
                  {n}
                </option>
              )}
            </For>
          </select>
        </label>
        <label class="stack small" style={{ gap: "2px" }}>
          Topic
          <input type="text" maxLength={THREAD_TOPIC_MAX} value={topic()} onInput={(e) => setTopic(e.currentTarget.value)} aria-label="Topic" placeholder="One line" />
        </label>
        <label class="stack small" style={{ gap: "2px" }}>
          First message (optional, Markdown)
          <textarea rows={5} value={text()} onInput={(e) => setText(e.currentTarget.value)} aria-label="First message" />
        </label>
        <p class="muted small" style={{ margin: 0 }}>
          The thread reports to the lead of its scope. The daemon does not start an agent from the template yet: pick an agent to attach one now.
        </p>
        <Show when={templates.loading}>
          <span class="muted small">Loading the templates…</span>
        </Show>
        <div class="row" style={{ gap: "6px" }}>
          <button class="primary" type="submit" disabled={busy()}>
            Open the thread
          </button>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}
