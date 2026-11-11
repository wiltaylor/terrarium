import { useInstance } from "../../instance";
// Ask an agent a question from the Inbox, or forward a question, bundle, or
// notice of the Inbox to one. New requests get a dedicated thread agent.
// Forwarding an existing question keeps its watched-mail contract.

import { createSignal, For, Show } from "solid-js";
import type { InboxAsk, Question, Thread } from "../../api/types";
import { ErrorBanner, Modal } from "../../components/ui";

import { ASK_TEXT_MAX, askBody } from "./model";
import { useDefaultAgent } from "./defaultAgent";

export function AskDialog(props: { question?: Question; onClose: () => void; onAsked: (r: InboxAsk) => void; onRequested?: (r: Thread) => void }) {
  const { agents, api } = useInstance();
  const forward = () => !!props.question;
  const defaultAgent = useDefaultAgent();
  const [picked, setTo] = createSignal<string>();
  const to = () => picked() ?? (choices().includes(defaultAgent() ?? "") ? defaultAgent()! : "");
  const [text, setText] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const choices = () =>
    agents.state.agents
      .filter((a) => forward() ? !a.ephemeral : !a.ephemeral && a.roles.includes("project_admin"))
      .map((a) => `${a.project}/${a.name}`)
      .sort();
  const submit = async (e: Event) => {
    e.preventDefault();
    const b = askBody(to(), text(), props.question?.id);
    if ("error" in b) return setError(b.error);
    setBusy(true);
    try {
      if (forward()) props.onAsked(await api.inbox.ask(b.body));
      else {
        const thread = await api.threads.open({ report_to: b.body.to, topic: b.body.text!.split("\n")[0]!.slice(0, 200), text: b.body.text });
        props.onRequested?.(thread);
      }
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal title={forward() ? "Forward to an agent" : "Ask an agent"} onClose={() => props.onClose()}>
      <form class="stack" style={{ gap: "8px" }} onSubmit={submit} aria-label={forward() ? "Forward" : "Ask"}>
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <Show when={props.question}>
          {(q) => (
            <div class="small">
              <span class="muted">
                The {q().kind === "bundle" ? "bundle of questions" : q().kind} of {q().agent_name}:
              </span>
              <p style={{ margin: "4px 0 0", "white-space": "pre-wrap", "font-weight": 600 }}>{q().text}</p>
              <p class="muted" style={{ margin: "4px 0 0" }}>
                It stays open here: you still answer it in the Inbox. The agent's reply comes to your Inbox, with "Use as answer".
              </p>
            </div>
          )}
        </Show>
        <label class="stack small" style={{ gap: "2px" }}>
          Agent
          <select value={to()} onChange={(e) => setTo(e.currentTarget.value)} aria-label="Agent">
            <option value="">Pick an agent…</option>
            <For each={choices()}>
              {(n) => (
                <option value={n} selected={n === to()}>
                  {n}
                </option>
              )}
            </For>
          </select>
        </label>
        <label class="stack small" style={{ gap: "2px" }}>
          {forward() ? "A note to the agent (optional)" : "The question"}
          <textarea rows={4} maxLength={ASK_TEXT_MAX} value={text()} onInput={(e) => setText(e.currentTarget.value)} aria-label={forward() ? "Note" : "Question text"} />
        </label>
        <p class="muted small" style={{ margin: 0 }}>
          {forward() ? "The agent gets one mail. The Asked tab shows it until it answers." : "A request agent opens a thread, passes your question to the selected lead, and tracks the answer and linked cards there."}
        </p>
        <div class="row" style={{ gap: "6px" }}>
          <button class="primary" type="submit" disabled={busy()}>
            {forward() ? "Forward" : "Ask"}
          </button>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}
