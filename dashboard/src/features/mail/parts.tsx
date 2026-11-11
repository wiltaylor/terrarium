import { useInstance } from "../../instance";
// Mail pieces that the mail log and the Inbox share. A subject and a body
// are the sender's words: they go into the page as text nodes only.

import { createSignal, For, Show } from "solid-js";
import { SessionLink } from "../../components/SessionLink";
import { ErrorBanner, Modal } from "../../components/ui";
import type { Message } from "../../api/types";
import { ForumPost, quotePost } from "../../components/ForumPost";
import { SafeHtml } from "../../components/SafeHtml";
import { renderMarkdown } from "../../lib/markdown";


/** The delivery state of a message, as chips. The state is the recipient's; the log keeps no unread state for the operator. */
export function StateChips(props: { message: Message }) {
  const m = () => props.message;
  return (
    <span class="chips">
      <span class={`chip ${m().state === "replied" ? "ok" : ""}`} title={m().delivered_at ? `delivered ${m().delivered_at}` : undefined}>
        {m().state}
      </span>
      <Show when={m().read_by_name ?? m().read_by}>
        {(who) => (
          <span class="chip" title={m().read_at ? `read ${m().read_at}` : undefined}>
            read by {who()}
          </span>
        )}
      </Show>
      <Show when={m().released_at}>
        {(at) => (
          <span class="chip" title={`The loop guard held it. ${m().released_by ?? "The operator"} released it ${at()}.`}>
            released
          </span>
        )}
      </Show>
      <Show when={m().thread_paused}>
        <span class="chip warn" title="The loop guard paused the thread. Resume it in the Inbox.">
          thread paused
        </span>
      </Show>
    </span>
  );
}

export function MessageView(props: { message: Message; number?: number; project?: string; onQuote?: (text: string) => void }) {
  return (
    <ForumPost id={`mail-${props.message.id}`} number={props.number ?? 1} href={`/logs/mail?open=${encodeURIComponent(props.message.id)}#mail-${props.message.id}`} author={props.message.from_name} at={props.message.created_at} label="Mail" onQuote={props.onQuote ? (post) => props.onQuote?.(quotePost(props.message.body, post)) : undefined}>
      <div class="row small" style={{ gap: "6px", "flex-wrap": "wrap" }}>
        <span title={props.message.to}>To {props.message.to_name}</span>
        <SessionLink row={props.message} />
        <Show when={props.message.subject}>
          <span class="muted">{props.message.subject}</span>
        </Show>
        <StateChips message={props.message} />
      </div>
      <SafeHtml class="prose" html={renderMarkdown(props.message.body, props.project ?? "", { textBody: true })} />
    </ForumPost>
  );
}

export function Compose(props: { to?: string; onClose: () => void; onSent: (m: Message) => void }) {
  const { api, agents, projects } = useInstance();
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
            const sent = await api.mail.send({ to: to().trim(), subject: subject().trim() || undefined, body: body() });
            props.onSent(sent);
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
