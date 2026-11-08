// Ask an agent a question from the Inbox, or forward a question, bundle, or
// notice of the Inbox to one (GitHub #24, docs/api.md §6.9 "Ask and
// forward"). The daemon sends one mail as the operator on a new thread and
// watches it: the Asked tab shows it until the agent answers.

import { createSignal, For, Show } from "solid-js";
import type { InboxAsk, Question } from "../../api/types";
import { ErrorBanner, Modal } from "../../components/ui";
import { agents, api } from "../../stores";
import { ASK_TEXT_MAX, askBody } from "./model";
import { useDefaultAgent } from "./defaultAgent";

export function AskDialog(props: { question?: Question; onClose: () => void; onAsked: (r: InboxAsk) => void }) {
  const forward = () => !!props.question;
  const defaultAgent = useDefaultAgent();
  const [picked, setTo] = createSignal<string>();
  const to = () => picked() ?? (choices().includes(defaultAgent() ?? "") ? defaultAgent()! : "");
  const [text, setText] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const choices = () =>
    agents.state.agents
      .filter((a) => !a.ephemeral || `${a.project}/${a.name}` === defaultAgent())
      .map((a) => `${a.project}/${a.name}`)
      .sort();
  const submit = async (e: Event) => {
    e.preventDefault();
    const b = askBody(to(), text(), props.question?.id);
    if ("error" in b) return setError(b.error);
    setBusy(true);
    try {
      props.onAsked(await api.inbox.ask(b.body));
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
          The agent gets one mail from you and answers with <code>terrarium mail reply</code>. The Asked tab shows the thread until it answers.
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
