import { useInstance } from "../../instance";
// The Compact and New session dialogs of the agent page. Each asks for an
// optional "then" prompt, which the daemon sends as the next turn after the
// reset. The New session dialog also has "Include recap" (default on).

import { createSignal, Show } from "solid-js";
import { ErrorBanner, Modal } from "../../components/ui";
import type { Agent, ResetKind, ResetResult } from "../../api/types";

import { resetBody, resetThenProblem } from "../../lib/lifecycle";

const PLACEHOLDER: Record<ResetKind, string> = {
  compact:
    "Optional. The daemon sends this prompt as the next turn after the compaction. The compaction keeps only a summary: restate the goal and the next step.",
  new_session:
    "Optional. The daemon sends this prompt as the next turn in the new session. A new session forgets the conversation: restate the goal, the card, and the next step.",
};

/** The dialog of a reset. `onDone` gets the 202 answer; the dialog then closes. */
export function ResetDialog(props: { agent: Agent; kind: ResetKind; title: string; onClose: () => void; onDone: (r: ResetResult) => void }) {
  const { api } = useInstance();
  const [then, setThen] = createSignal("");
  const [recap, setRecap] = createSignal(true);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const problem = () => resetThenProblem(then());
  const submit = async (e: Event) => {
    e.preventDefault();
    if (problem()) return;
    setBusy(true);
    try {
      const body = resetBody(props.kind, then(), recap());
      const r = props.kind === "compact" ? await api.agents.compact(props.agent.id, body) : await api.agents.newSession(props.agent.id, body);
      props.onDone(r);
      props.onClose();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal title={`${props.title}: ${props.agent.name}`} onClose={() => props.onClose()}>
      <form class="stack" onSubmit={submit} aria-label={props.title}>
        <p class="small muted" style={{ margin: 0 }}>
          {props.kind === "compact"
            ? "The harness summarises the conversation (one model call). A busy agent compacts when its current turn ends."
            : "The daemon ends the session and starts a new, clean harness session. A busy agent starts it when its current turn ends."}
        </p>
        <label>
          Then send…
          <textarea rows={6} value={then()} placeholder={PLACEHOLDER[props.kind]} onInput={(e) => setThen(e.currentTarget.value)} />
        </label>
        <Show when={problem()}>
          <p class="small" role="alert" style={{ color: "var(--danger)", margin: 0 }}>
            {problem()}
          </p>
        </Show>
        <Show when={props.kind === "new_session"}>
          <label class="row small" style={{ "flex-wrap": "nowrap", "align-items": "flex-start" }}>
            <input type="checkbox" checked={recap()} onChange={(e) => setRecap(e.currentTarget.checked)} />
            <span>Include recap: the first turn lists the open cards, waits, unread mail, and open questions</span>
          </label>
        </Show>
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <div class="row" style={{ "justify-content": "flex-end" }}>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
          <button class="primary" type="submit" disabled={busy() || !!problem()}>
            {busy() ? "Sending…" : props.title}
          </button>
        </div>
      </form>
    </Modal>
  );
}
