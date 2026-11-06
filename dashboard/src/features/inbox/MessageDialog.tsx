// Message an agent from the Inbox (TER-46): one mail from the operator to an
// agent, through `POST /api/mail` (the call of the Mail page). Unlike "Ask an
// agent", nothing is watched and nothing waits: no Asked item, no badge. A
// reply of the agent comes in as a normal mail item.

import { createSignal, For } from "solid-js";
import type { Message } from "../../api/types";
import { ErrorBanner, Modal } from "../../components/ui";
import { agents, api } from "../../stores";
import { ASK_TEXT_MAX, messageBody } from "./model";

export function MessageDialog(props: { onClose: () => void; onSent: (m: Message) => void }) {
  const [to, setTo] = createSignal("");
  const [text, setText] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const choices = () =>
    agents.state.agents
      .filter((a) => !a.ephemeral)
      .map((a) => `${a.project}/${a.name}`)
      .sort();
  const submit = async (e: Event) => {
    e.preventDefault();
    const b = messageBody(to(), text());
    if ("error" in b) return setError(b.error);
    setBusy(true);
    try {
      props.onSent(await api.mail.send(b.body));
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
          The agent gets one mail from you; the first line is the subject. Nothing waits for an answer. A reply comes to your Inbox as mail.
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
