// The cards of the Inbox page that are not questions: mail to the operator,
// a thread that the loop guard paused, and a watch (an ask or a forward
// that waits for an agent). A mail body and a subject are the
// sender's words: they go into the page as text nodes only.

import { A } from "@solidjs/router";
import { createSignal, For, Show } from "solid-js";
import type { InboxWatch, Message, PausedThread } from "../../api/types";
import { ApiError } from "../../api/http";
import { ConfirmButton, ErrorBanner } from "../../components/ui";
import { ago } from "../../lib/format";
import { api } from "../../stores";
import { MessageView } from "../mail/parts";
import { answerLine, systemMailLink, watchState, watchTitle } from "./model";

/** Run one call with a busy flag, and keep its error for the banner. */
function useAction() {
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const run = async (call: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await call();
      setError(null);
      return true;
    } catch (e) {
      setError(e);
      return false;
    } finally {
      setBusy(false);
    }
  };
  return { busy, error, setError, run };
}

/** The Mail tab of the logs, filtered to the thread, with the thread open. */
const threadHref = (thread: string) => `/logs/mail?thread=${encodeURIComponent(thread)}&open=${encodeURIComponent(thread)}`;

/**
 * A mail to `operator`: Acknowledge, with an optional reply on the thread.
 * On a watched thread (`watch`) it says what it answers. The answer to a
 * forward links the question (`onOpenQuestion`) and has "Use as answer"
 * (`onUseAsAnswer`: it fills the answer box of the question).
 */
export function MailItemCard(props: {
  message: Message;
  open: boolean;
  now: number;
  onAck: (m: Message) => void;
  watch?: InboxWatch;
  onOpenQuestion?: (questionId: string) => void;
  onUseAsAnswer?: (questionId: string, text: string) => void;
}) {
  const m = () => props.message;
  const [reply, setReply] = createSignal("");
  const a = useAction();
  const system = () => m().from_kind === "system";
  const link = () => systemMailLink(m());
  const ack = async (e: Event) => {
    e.preventDefault();
    const text = reply().trim();
    let done: Message | undefined;
    // A reply to a system mail with nobody else on the thread is a 422; the banner shows it.
    // A 409 means the mail was read or replied outside the Inbox (the CLI, the
    // Mail page), so the item is closed already. Without text the ack is done:
    // fetch the message as it is now, so the card under Done shows the real
    // state (GitHub #25). With text the reply was not sent (the daemon checks
    // the state first), so the 409 reaches the banner and the text stays.
    if (
      await a.run(async () => {
        try {
          done = (await api.inbox.ackMail(m().id, text || undefined)).message;
        } catch (e) {
          if (!(e instanceof ApiError && e.status === 409)) throw e;
          if (text) throw e;
          done = await api.mail.get(m().id);
        }
      })
    ) {
      setReply("");
      if (done) props.onAck(done);
    }
  };
  return (
    <article class="card-box stack" style={{ gap: "8px" }} aria-label={`Mail ${m().id}`}>
      <div class="row small" style={{ "flex-wrap": "wrap", gap: "6px" }}>
        <span class="chip">mail</span>
        <span class={`chip ${props.open ? "info" : "ok"}`}>{props.open ? "open" : m().state === "replied" ? "replied" : "acknowledged"}</span>
        <span>
          from <strong title={m().from}>{m().from_name}</strong>
        </span>
        <span class="muted" title={m().created_at}>
          {ago(m().created_at, props.now)}
        </span>
        <A href={threadHref(m().thread_id)} class="muted">
          open the thread
        </A>
      </div>
      <div class="spread" style={{ gap: "8px", "flex-wrap": "wrap" }}>
        <p style={{ margin: 0, "font-weight": 600 }}>{m().subject || "(no subject)"}</p>
        <Show when={link()}>
          {(l) => (
            <A class="btn small" href={l().href}>
              {l().label}
            </A>
          )}
        </Show>
      </div>
      <Show when={props.watch && m().from_kind === "agent" ? props.watch : undefined}>
        {(w) => (
          <div class="row small" style={{ "flex-wrap": "wrap", gap: "6px" }} data-testid="watch-answer">
            <span class="chip info">answer</span>
            <Show when={w().question_id} fallback={<span>{answerLine(w())}</span>}>
              {(qid) => (
                <>
                  <button type="button" class="link small" onClick={() => props.onOpenQuestion?.(qid())} title="Open the forwarded question">
                    {answerLine(w())}
                  </button>
                  <button type="button" class="small" onClick={() => props.onUseAsAnswer?.(qid(), m().body)} title="Fill the answer box of the question with this reply">
                    Use as answer
                  </button>
                </>
              )}
            </Show>
          </div>
        )}
      </Show>
      <pre class="mail-body">{m().body}</pre>
      <Show when={props.open}>
        <form class="stack" style={{ gap: "6px" }} onSubmit={ack} aria-label="Acknowledge">
          <ErrorBanner error={a.error()} onDismiss={() => a.setError(null)} />
          <textarea
            rows={2}
            placeholder={system() ? "A reply to the other participants of the thread (optional)" : `A reply to ${m().from_name} (optional)`}
            value={reply()}
            onInput={(e) => setReply(e.currentTarget.value)}
            aria-label="Reply text"
          />
          <div>
            <button class="primary" type="submit" disabled={a.busy()}>
              {reply().trim() ? "Acknowledge and reply" : "Acknowledge"}
            </button>
          </div>
        </form>
      </Show>
    </article>
  );
}

/** A thread that the loop guard paused: release the held messages one at a time, or resume the thread. */
export function PausedThreadCard(props: { system: PausedThread; onChange: () => void }) {
  const s = () => props.system;
  const [note, setNote] = createSignal("");
  const a = useAction();
  const release = async (m: Message) => {
    if (await a.run(() => api.mail.release(m.id))) props.onChange();
  };
  const resume = async (e: Event) => {
    e.preventDefault();
    if (await a.run(() => api.mail.resume(s().thread_id, note().trim() || undefined))) {
      setNote("");
      props.onChange();
    }
  };
  return (
    <article class="card-box stack" style={{ gap: "8px" }} aria-label={`Thread ${s().thread_id}`}>
      <div class="row small" style={{ "flex-wrap": "wrap", gap: "6px" }}>
        <span class="chip">system</span>
        <Show when={s().paused} fallback={<span class="chip">not paused</span>}>
          <span class="chip warn" title="The daemon types no mail pointers for this thread">
            thread paused
          </span>
        </Show>
        <span title="Agent-to-agent messages since the last reset, and the limit (TERRARIUM_MAIL_LOOP_LIMIT)">
          {s().agent_run} of {s().limit} agent messages
        </span>
        <A href={threadHref(s().thread_id)} class="muted">
          open the thread
        </A>
        <span class="mono muted">{s().thread_id}</span>
      </div>
      <p style={{ margin: 0, "font-weight": 600 }}>{s().subject || "(no subject)"}</p>
      <div class="small">
        Between: <For each={s().participants}>{(p, i) => (i() ? <>, {p}</> : p)}</For>
      </div>
      <ErrorBanner error={a.error()} onDismiss={() => a.setError(null)} />
      <Show when={s().held.length} fallback={<p class="muted small" style={{ margin: 0 }}>No held messages.</p>}>
        <div class="stack" style={{ gap: "6px" }}>
          <span class="small muted">Held messages: the recipients did not get them.</span>
          <For each={s().held}>
            {(m) => (
              <div class="stack" style={{ gap: "4px" }}>
                <MessageView message={m} />
                <div>
                  <button class="small" disabled={a.busy()} onClick={() => void release(m)} title="The recipient gets this message now. The thread stays paused.">
                    Release
                  </button>
                </div>
              </div>
            )}
          </For>
        </div>
      </Show>
      <form class="stack" style={{ gap: "6px" }} onSubmit={resume} aria-label="Resume">
        <textarea rows={2} placeholder="A note to the participants (optional)" value={note()} onInput={(e) => setNote(e.currentTarget.value)} aria-label="Note" />
        <div>
          <button class="primary" type="submit" disabled={a.busy()} title="End the pause and release every held message">
            Resume the thread
          </button>
        </div>
      </form>
    </article>
  );
}

/**
 * A watch (docs/api.md §6.9): a question that the operator asked an agent,
 * or forwarded to one. "Waiting for p/y: <subject>" while the operator spoke
 * last; Cancel stops waiting (the mail stays).
 */
export function WatchCard(props: { watch: InboxWatch; now: number; onChange: (w: InboxWatch) => void }) {
  const w = () => props.watch;
  const a = useAction();
  const st = () => watchState(w());
  const cancel = async () => {
    let next: InboxWatch | undefined;
    if (await a.run(async () => (next = await api.inbox.cancelWatch(w().thread_id))) && next) props.onChange(next);
  };
  return (
    <article class="card-box stack" style={{ gap: "8px" }} aria-label={`Watch ${w().thread_id}`}>
      <div class="row small" style={{ "flex-wrap": "wrap", gap: "6px" }}>
        <span class="chip">{w().question_id ? "forwarded" : "asked"}</span>
        <span class={`chip ${st().chip}`}>{st().label}</span>
        <A href={`/agents/${encodeURIComponent(w().agent)}`}>{w().agent_name}</A>
        <span class="muted" title={w().created_at}>
          asked {ago(w().created_at, props.now)}
        </span>
        <Show when={w().replies}>
          <span class="muted">
            {w().replies} {w().replies === 1 ? "reply" : "replies"}
          </span>
        </Show>
        <A href={threadHref(w().thread_id)} class="muted">
          open the thread
        </A>
      </div>
      <p style={{ margin: 0, "font-weight": 600 }}>{watchTitle(w())}</p>
      <Show when={w().question_id}>
        {(q) => <span class="small muted">The forwarded question {q()} stays open in the Inbox.</span>}
      </Show>
      <ErrorBanner error={a.error()} onDismiss={() => a.setError(null)} />
      <Show when={!w().cancelled_at}>
        <div>
          <ConfirmButton class="small" label="Cancel" confirmLabel="Yes, stop waiting" disabled={a.busy()} onConfirm={() => void cancel()} />
        </div>
      </Show>
    </article>
  );
}
