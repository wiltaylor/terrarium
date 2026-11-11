// Message a lead through a dedicated request thread (TER-687).

import { createSignal, For } from "solid-js";
import type { Thread } from "../../api/types";
import { ErrorBanner, Modal } from "../../components/ui";
import { agents, api } from "../../stores";
import { ASK_TEXT_MAX, messageBody } from "./model";
import { useDefaultAgent } from "./defaultAgent";

export function MessageDialog(props: { onClose: () => void; onSent: (m: Thread) => void }) {
  const defaultAgent = useDefaultAgent();
  const [picked, setTo] = createSignal<string>();
  const to = () => picked() ?? (choices().includes(defaultAgent() ?? "") ? defaultAgent()! : "");
  const [text, setText] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const choices = () =>
    agents.state.agents
      .filter((a) => !a.ephemeral && a.roles.includes("project_admin"))
      .map((a) => `${a.project}/${a.name}`)
      .sort();
  const submit = async (e: Event) => {
    e.preventDefault();
    const b = messageBody(to(), text());
    if ("error" in b) return setError(b.error);
    setBusy(true);
    try {
      props.onSent(await api.threads.open({ report_to: b.body.to, topic: b.body.subject.slice(0, 200), text: b.body.body }));
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal title="Message an agent" onClose={() => props.onClose()}>
      <form class="stack" style={{ gap: "8px" }} onSubmit={submit} aria-label="Message">
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
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
          The message
          <textarea rows={4} maxLength={ASK_TEXT_MAX} value={text()} onInput={(e) => setText(e.currentTarget.value)} aria-label="Message text" />
        </label>
        <p class="muted small" style={{ margin: 0 }}>
          A request agent opens a thread, passes your message to the selected lead, and tracks the answer and linked cards there.
        </p>
        <div class="row" style={{ gap: "6px" }}>
          <button class="primary" type="submit" disabled={busy()}>
            Send
          </button>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}
