// The agent's close proposal of an Inbox thread (TER-95, docs/api.md §6.13):
// the proposed handoff (summary and links) with Confirm and Decline, on the
// chat page and in the Threads list. Confirm is `POST .../close` with
// `{confirm: true}`: the thread closes with the agent's handoff. Decline is a
// message of the operator: it clears the proposal, and the agent goes on.
//
// The summary and the link refs are the agent's words: text nodes only. A
// link is a link only for http and https.

import { createSignal, For, Show } from "solid-js";
import { ApiError } from "../../api/http";
import type { Thread, ThreadLink } from "../../api/types";
import { ErrorBanner } from "../../components/ui";
import { ago } from "../../lib/format";
import { api } from "../../stores";
import { pendingProposal, retractedOnClose, safeUrl, THREAD_MESSAGE_MAX, threadText, whoName } from "./threads";

/** The links of a handoff or a proposal. */
export function HandoffLinks(props: { links: ThreadLink[] }) {
  return (
    <Show when={props.links.length}>
      <ul class="small" style={{ margin: 0 }}>
        <For each={props.links}>
          {(l) => (
            <li>
              <span class="chip">{l.kind}</span>{" "}
              <Show when={safeUrl(l)} fallback={<span>{l.ref}</span>}>
                {(u) => (
                  <a href={u()} target="_blank" rel="noopener noreferrer">
                    {l.ref}
                  </a>
                )}
              </Show>
            </li>
          )}
        </For>
      </ul>
    </Show>
  );
}

/**
 * The pending close proposal of a thread with Confirm and Decline, or
 * nothing. `onChanged` gets the thread that a confirm or a decline returns;
 * `onError` gets each failed write (a 409: the thread changed elsewhere).
 */
export function CloseProposal(props: { thread: Thread; now: number; onChanged: (t: Thread) => void; onError?: (e: unknown) => void }) {
  const [declining, setDeclining] = createSignal(false);
  const [text, setText] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const open = () => retractedOnClose(props.thread);
  const run = async (write: () => Promise<Thread>) => {
    setBusy(true);
    try {
      const t = await write();
      setError(null);
      setDeclining(false);
      setText("");
      props.onChanged(t);
    } catch (err) {
      setError(err);
      props.onError?.(err);
    } finally {
      setBusy(false);
    }
  };
  const confirm = () => run(() => api.threads.close(props.thread.id, { confirm: true }));
  const decline = (e: Event) => {
    e.preventDefault();
    const t = threadText(text(), THREAD_MESSAGE_MAX, "message");
    if ("error" in t) return setError(t.error);
    void run(() => api.threads.post(props.thread.id, t.text));
  };
  return (
    <Show when={pendingProposal(props.thread)}>
      {(p) => (
        <section class="card-box stack thread-proposal" style={{ gap: "6px" }} aria-label="Close proposal">
          <span class="small">
            <strong>{whoName(props.thread, p().by)}</strong> proposes to close the thread{" "}
            <span class="muted" title={p().at}>
              {ago(p().at, props.now)}
            </span>
            .
          </span>
          <ErrorBanner error={error()} onDismiss={() => setError(null)} />
          <Show when={p().summary} fallback={<span class="small muted">No handoff summary.</span>}>
            {(s) => (
              <p style={{ margin: 0, "white-space": "pre-wrap" }} data-testid="proposal-summary">
                {s()}
              </p>
            )}
          </Show>
          <HandoffLinks links={p().links} />
          <Show when={open() > 0}>
            <p class="small warn-text" style={{ margin: 0 }} data-testid="proposal-retracts">
              A confirm retracts {open()} open {open() === 1 ? "question" : "questions"}.
            </p>
          </Show>
          <Show
            when={declining()}
            fallback={
              <div class="row" style={{ gap: "6px" }}>
                <button type="button" class="primary" disabled={busy()} onClick={() => void confirm()}>
                  Confirm
                </button>
                <button type="button" disabled={busy()} onClick={() => (setDeclining(true), setError(null))}>
                  Decline
                </button>
              </div>
            }
          >
            <form class="stack" style={{ gap: "6px" }} onSubmit={decline} aria-label="Decline the close">
              <span class="small muted">Your message declines the proposal. The thread stays open, and the agent goes on.</span>
              <textarea rows={3} placeholder="What more is needed? (Markdown)" value={text()} onInput={(e) => setText(e.currentTarget.value)} aria-label="Decline text" />
              <div class="row" style={{ gap: "6px" }}>
                <button class="primary" type="submit" disabled={busy()}>
                  Send and decline
                </button>
                <button type="button" onClick={() => (setDeclining(false), setError(null))}>
                  Cancel
                </button>
              </div>
            </form>
          </Show>
        </section>
      )}
    </Show>
  );
}

/** Did a write fail because the thread changed elsewhere? */
export const isConflict = (e: unknown): boolean => e instanceof ApiError && e.status === 409;
