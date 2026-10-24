import { createEffect, createMemo, createSignal, For, on, onCleanup, Show } from "solid-js";
import { createPagedList } from "../../components/paged";
import { Empty, ErrorBanner, LoadMore, Modal, PageHead } from "../../components/ui";
import type { Message, MessageState } from "../../api/types";
import { ago } from "../../lib/format";
import { agents, api, events, projects, unread } from "../../stores";
import { groupThreads } from "./threads";

export function MessageView(props: { message: Message }) {
  return (
    <div class="message">
      <div class="spread small">
        <span>
          <strong title={props.message.from}>{props.message.from_name}</strong> → <span title={props.message.to}>{props.message.to_name}</span>
        </span>
        <span class="muted">
          {props.message.state} · {ago(props.message.created_at)}
        </span>
      </div>
      <Show when={props.message.subject}>
        <div class="small muted">{props.message.subject}</div>
      </Show>
      <pre>{props.message.body}</pre>
    </div>
  );
}

export function Compose(props: { to?: string; onClose: () => void; onSent: () => void }) {
   
  const [to, setTo] = createSignal(props.to ?? "");
  const [subject, setSubject] = createSignal("");
  const [body, setBody] = createSignal("");
  const [error, setError] = createSignal<unknown>(null);
  return (
    <Modal title="Compose as operator" onClose={() => props.onClose()}>
      <ErrorBanner error={error()} />
      <form
        class="stack"
        onSubmit={async (e) => {
          e.preventDefault();
          try {
            await api.mail.send({ to: to().trim(), subject: subject().trim() || undefined, body: body() });
            props.onSent();
            props.onClose();
          } catch (err) {
            setError(err);
          }
        }}
      >
        <label>
          To (project/agent, project:&lt;name&gt;, or operator)
          <input list="mail-recipients" value={to()} onInput={(e) => setTo(e.currentTarget.value)} required />
          <datalist id="mail-recipients">
            <For each={agents.state.agents}>{(a) => <option value={`${a.project}/${a.name}`}>{a.harness}</option>}</For>
            <For each={projects.projects()}>{(p) => <option value={`project:${p.name}`}>project inbox</option>}</For>
          </datalist>
        </label>
        <label>
          Subject
          <input value={subject()} onInput={(e) => setSubject(e.currentTarget.value)} />
        </label>
        <label>
          Body
          <textarea value={body()} onInput={(e) => setBody(e.currentTarget.value)} required />
        </label>
        <div>
          <button class="primary" type="submit">
            Send
          </button>
        </div>
      </form>
    </Modal>
  );
}

export default function MailPage() {
  const [state, setState] = createSignal<MessageState | "unread" | "">("");
  const [error, setError] = createSignal<unknown>(null);
  const mail = createPagedList(
    () => state() || "all",
    (_k, cursor) => api.mail.list({ state: state() || undefined, cursor }),
    (m) => m.id,
  );
  const threads = createMemo(() => groupThreads(mail.items()));
  const [active, setActive] = createSignal<string | null>(null);
  const [composing, setComposing] = createSignal(false);
  const [replyText, setReplyText] = createSignal("");
  const thread = () => threads().find((t) => t.id === (active() ?? threads()[0]?.id));

  // Live: a mail event reloads the first page.
  let t: ReturnType<typeof setTimeout> | undefined;
  const off = events.on((e) => {
    if (e.kind !== "mail") return;
    clearTimeout(t);
    t = setTimeout(() => void mail.reload(), 300);
  });
  onCleanup(() => (off(), clearTimeout(t)));

  // Opening a thread marks the operator's unread messages on it as read.
  createEffect(
    on(thread, (th) => {
      const todo = th?.messages.filter((m) => m.to === "operator" && (m.state === "queued" || m.state === "delivered")) ?? [];
      if (!todo.length) return;
      void Promise.all(todo.map((m) => api.mail.markRead(m.id).catch(() => null))).then((done) => {
        mail.upsert(done.filter((m): m is Message => !!m));
        void unread.refresh();
      });
    }),
  );

  return (
    <>
      <PageHead title="Mail">
        <select value={state()} onChange={(e) => setState(e.currentTarget.value as MessageState | "unread" | "")} aria-label="State">
          <option value="">Any state</option>
          <option value="unread">unread</option>
          <option value="queued">queued</option>
          <option value="delivered">delivered</option>
          <option value="read">read</option>
          <option value="replied">replied</option>
        </select>
        <button onClick={() => mail.reload()}>Refresh</button>
        <button class="primary" onClick={() => setComposing(true)}>
          Compose
        </button>
      </PageHead>
      <ErrorBanner error={error() ?? mail.error()} onDismiss={() => setError(null)} />
      <div class="split">
        <div class="card-box thread-list" style={{ padding: "4px" }}>
          <For each={threads()} fallback={<Empty>{mail.initial() ? "Loading…" : "No mail."}</Empty>}>
            {(th) => (
              <button class={`${thread()?.id === th.id ? "active" : ""} ${th.unread ? "unread" : ""}`} onClick={() => setActive(th.id)}>
                <span class="subject">{th.subject || "(no subject)"}</span>
                <span class="muted small">
                  {th.participants.join(", ")} · {ago(th.last_at)}
                </span>
              </button>
            )}
          </For>
          <LoadMore hasMore={mail.hasMore()} loading={mail.loading()} onMore={mail.loadMore} count={mail.items().length} />
        </div>
        <Show when={thread()} fallback={<Empty>Pick a thread.</Empty>}>
          {(th) => (
            <div class="stack">
              <h2 style={{ margin: 0 }}>{th().subject || "(no subject)"}</h2>
              <For each={th().messages}>{(m) => <MessageView message={m} />}</For>
              <form
                class="stack"
                style={{ gap: "6px" }}
                onSubmit={async (e) => {
                  e.preventDefault();
                  const last = th().messages.at(-1);
                  if (!last || !replyText().trim()) return;
                  try {
                    // One message for each participant (docs/api.md §7.2).
                    const sent = await api.mail.reply(last.id, replyText());
                    mail.upsert(sent);
                    setReplyText("");
                    setError(null);
                  } catch (err) {
                    setError(err);
                  }
                }}
              >
                <textarea placeholder="Reply as operator, to every participant" value={replyText()} onInput={(e) => setReplyText(e.currentTarget.value)} aria-label="Reply" />
                <div>
                  <button class="primary" type="submit" disabled={!replyText().trim()}>
                    Reply
                  </button>
                </div>
              </form>
            </div>
          )}
        </Show>
      </div>
      <Show when={composing()}>
        <Compose onClose={() => setComposing(false)} onSent={() => void mail.reload()} />
      </Show>
    </>
  );
}
