// Mail pieces that the mail log and the project inbox share.

import { createSignal, For, Show } from "solid-js";
import { ErrorBanner, Modal } from "../../components/ui";
import type { Message } from "../../api/types";
import { ago } from "../../lib/format";
import { agents, api, projects } from "../../stores";
import { isUnread, REASON_LABELS, type Notice, type Reason } from "./log";

const NOTICE_LABELS: Record<Notice, string> = { blocked: "blocked card", quota: "quota", needs_human: "needs a human", login: "login expired", notice: "system" };

/** The state of a message, and what the log knows about it, as chips. */
export function StateChips(props: { message: Message; held?: boolean; notice?: Notice | null; reasons?: Reason[] }) {
  const m = () => props.message;
  return (
    <span class="chips">
      <Show when={isUnread(m()) && !props.reasons?.includes("unread")}>
        <span class="chip info" title={`${m().to_name} has not read it`}>
          unread
        </span>
      </Show>
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
      <Show when={props.notice && props.notice !== "notice"}>
        <span class="chip warn">{NOTICE_LABELS[props.notice!]}</span>
      </Show>
      <Show when={props.held}>
        <span class="chip bad" title="The loop guard held it: delivered with no pointer to the agent">
          held
        </span>
      </Show>
      <Show when={m().thread_paused}>
        <span class="chip warn" title="The loop guard paused the thread. A reply from you resumes it.">
          thread paused
        </span>
      </Show>
      <Show when={props.reasons?.includes("unread")}>
        <span class="chip bad" title="Mail to you that you have not read">
          {REASON_LABELS.unread}
        </span>
      </Show>
    </span>
  );
}

export function MessageView(props: { message: Message; held?: boolean; notice?: Notice | null }) {
  return (
    <div class="message">
      <div class="spread small">
        <span>
          <strong title={props.message.from}>{props.message.from_name}</strong> → <span title={props.message.to}>{props.message.to_name}</span>
        </span>
        <span class="muted" title={props.message.created_at}>
          {ago(props.message.created_at)}
        </span>
      </div>
      <div class="row small" style={{ gap: "6px", "flex-wrap": "wrap" }}>
        <Show when={props.message.subject}>
          <span class="muted">{props.message.subject}</span>
        </Show>
        <StateChips message={props.message} held={props.held} notice={props.notice} />
      </div>
      <pre>{props.message.body}</pre>
    </div>
  );
}

export function Compose(props: { to?: string; onClose: () => void; onSent: (m: Message) => void }) {
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
