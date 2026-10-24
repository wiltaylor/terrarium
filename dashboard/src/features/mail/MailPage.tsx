import { createMemo, createResource, createSignal, For, Show } from "solid-js";
import { Empty, ErrorBanner, Modal, PageHead } from "../../components/ui";
import type { Message } from "../../api/types";
import { ago } from "../../lib/format";
import { agents, api } from "../../stores";
import { groupThreads } from "./threads";

export function MessageView(props: { message: Message }) {
  return (
    <div class="message">
      <div class="spread small">
        <span>
          <strong>{props.message.from}</strong> → {props.message.to}
        </span>
        <span class="muted">
          {props.message.state} · {ago(props.message.created_at)}
        </span>
      </div>
      <div class="small muted">{props.message.subject}</div>
      <pre>{props.message.body}</pre>
    </div>
  );
}

export function Compose(props: { to?: string; onClose: () => void; onSent: () => void }) {
   
  const [to, setTo] = createSignal(props.to ?? "");
  const [subject, setSubject] = createSignal("");
  const [body, setBody] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  return (
    <Modal title="Compose as operator" onClose={() => props.onClose()}>
      <ErrorBanner error={error()} />
      <form
        class="stack"
        onSubmit={async (e) => {
          e.preventDefault();
          try {
            await api.mail.send({ from: "operator", to: to().trim(), subject: subject().trim(), body: body() });
            props.onSent();
            props.onClose();
          } catch (err) {
            setError((err as Error).message);
          }
        }}
      >
        <label>
          To (agent name or project:&lt;name&gt;)
          <input list="mail-recipients" value={to()} onInput={(e) => setTo(e.currentTarget.value)} required />
          <datalist id="mail-recipients">
            <For each={agents.state.agents}>{(a) => <option value={a.name}>{a.project}</option>}</For>
          </datalist>
        </label>
        <label>
          Subject
          <input value={subject()} onInput={(e) => setSubject(e.currentTarget.value)} required />
        </label>
        <label>
          Body
          <textarea value={body()} onInput={(e) => setBody(e.currentTarget.value)} />
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
  const [mail, { refetch }] = createResource(() => api.mail.list());
  const threads = createMemo(() => groupThreads(mail() ?? []));
  const [active, setActive] = createSignal<string | null>(null);
  const [composing, setComposing] = createSignal(false);
  const [replyText, setReplyText] = createSignal("");
  const thread = () => threads().find((t) => t.id === (active() ?? threads()[0]?.id));

  return (
    <>
      <PageHead title="Mail">
        <button onClick={refetch}>Refresh</button>
        <button class="primary" onClick={() => setComposing(true)}>
          Compose
        </button>
      </PageHead>
      <div class="split">
        <div class="card-box thread-list" style={{ padding: "4px" }}>
          <For each={threads()} fallback={<Empty>No mail.</Empty>}>
            {(t) => (
              <button class={`${thread()?.id === t.id ? "active" : ""} ${t.unread ? "unread" : ""}`} onClick={() => setActive(t.id)}>
                <span class="subject">{t.subject}</span>
                <span class="muted small">
                  {t.participants.join(", ")} · {ago(t.last_at)}
                </span>
              </button>
            )}
          </For>
        </div>
        <Show when={thread()} fallback={<Empty>Pick a thread.</Empty>}>
          {(t) => (
            <div class="stack">
              <h2 style={{ margin: 0 }}>{t().subject}</h2>
              <For each={t().messages}>{(m) => <MessageView message={m} />}</For>
              <form
                class="stack"
                style={{ gap: "6px" }}
                onSubmit={async (e) => {
                  e.preventDefault();
                  const last = t().messages.at(-1);
                  if (!last || !replyText().trim()) return;
                  await api.mail.reply(last.id, replyText());
                  setReplyText("");
                  void refetch();
                }}
              >
                <textarea placeholder="Reply as operator" value={replyText()} onInput={(e) => setReplyText(e.currentTarget.value)} aria-label="Reply" />
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
        <Compose onClose={() => setComposing(false)} onSent={refetch} />
      </Show>
    </>
  );
}
