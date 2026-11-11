// One question or notice of a leader to the operator. A question has the
// answer form; a notice has Acknowledge with an optional reply. The text and
// the options are the agent's words: they go into the page as text nodes
// only. The context is Markdown, and goes through the DOMPurify sink
// (SafeHtml) only.

import { A, useNavigate } from "@solidjs/router";
import { createEffect, createSignal, For, type JSX, on, Show } from "solid-js";
import type { Question } from "../../api/types";
import { LinkText, openCardLink, readerCardKeys } from "../../components/LinkText";
import { SafeHtml } from "../../components/SafeHtml";
import { ConfirmButton, ErrorBanner } from "../../components/ui";
import { ago } from "../../lib/format";
import { renderMarkdown } from "../../lib/markdown";
import { api } from "../../stores";
import { ackText, answerBody, canOpenInThread, cardHref, chosenLabel, mailThreadHref, statusChip, THREAD_MOVE_MESSAGE_MAX } from "./questions";
import { threadHref } from "./threads";

/**
 * `prefill`: text for the answer box (the reply of an agent that the
 * operator forwarded the question to, "Use as answer"). A new object fills
 * it again. `inThread`: the card is on the page of its own thread, so it
 * has no "Open in thread".
 *
 * "Open in thread" (GitHub #32) opens the linked Inbox thread, or, for an
 * open question or notice with none, creates it with to-thread (TER-469)
 * and opens it. "Show mail" opens the mail thread of the question.
 */
export function QuestionCard(props: { question: Question; now: number; showAgent?: boolean; showKind?: boolean; hideCard?: boolean; inThread?: boolean; actions?: JSX.Element; prefill?: { text: string }; onChange: (q: Question) => void }) {
  const navigate = useNavigate();
  const q = () => props.question;
  const notice = () => q().kind === "notice";
  const [option, setOption] = createSignal<number | null>(null);
  const [selected, setSelected] = createSignal<number[]>([]);
  const [text, setText] = createSignal("");
  const [skip, setSkip] = createSignal(false);
  const [reason, setReason] = createSignal("");
  const [threadMessage, setThreadMessage] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  createEffect(on(() => props.prefill, (p) => p && setText(p.text)));

  const run = async (call: () => Promise<Question>) => {
    setBusy(true);
    try {
      props.onChange(await call());
      setError(null);
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };
  const send = (e: Event) => {
    e.preventDefault();
    if (notice()) {
      const t = ackText(q().status, text());
      if ("error" in t) return setError(t.error);
      return void run(() => api.questions.ack(q().id, t.text));
    }
    const b = answerBody(q(), text(), option(), skip(), selected());
    if ("error" in b) return setError(b.error);
    void run(() => api.questions.answer(q().id, b.body));
  };

  return (
    <article
      class={`card-box stack question${q().priority !== "normal" && q().status === "open" ? " urgent" : ""}${q().priority === "critical" && q().status === "open" ? " critical" : ""}`}
      style={{ gap: "8px" }}
      aria-label={`${notice() ? "Notice" : "Question"} ${q().id}`}
    >
      <div class="row small" style={{ "flex-wrap": "wrap", gap: "6px" }}>
        <Show when={props.showKind}>
          <span class="chip">{q().kind}</span>
        </Show>
        <Show when={q().priority !== "normal"}>
          <span class={`chip bad${q().priority === "critical" ? " strong" : ""}`}>{q().priority}</span>
        </Show>
        <span class={`chip ${statusChip(q().status)}`}>{q().status}</span>
        <Show when={props.showAgent}>
          <A href={`/agents/${encodeURIComponent(q().agent)}`}>{q().agent_name}</A>
        </Show>
        <A href={`/p/${encodeURIComponent(q().project)}/board`} class="muted">
          {q().project}
        </A>
        <Show when={!props.hideCard && q().card}>
          {(c) => (
            <A class="chip card-link" href={cardHref(q().project, c())} title={`Open card ${c()} on the board`}>
              card {c()}
            </A>
          )}
        </Show>
        <span class="muted" title={q().created_at}>
          {notice() ? "sent" : "asked"} {ago(q().created_at, props.now)}
        </span>
        <Show when={q().revision > 1}>
          <span class="muted" title={`Updated ${q().updated_at}`}>
            revision {q().revision}
          </span>
        </Show>
        <span class="mono muted">{q().id}</span>
        <Show when={q().thread_id}>
          {(m) => (
            <A href={mailThreadHref(m())} class="muted" title="The mail thread of this question">
              Show mail
            </A>
          )}
        </Show>
        {props.actions}
      </div>

      <p class="question-text" style={{ margin: 0, "white-space": "pre-wrap", "font-weight": 600 }}>
        <LinkText text={q().text} project={q().project} />
      </p>

      <Show when={q().context}>{(c) => <SafeHtml class="prose question-context" html={renderMarkdown(c(), q().project, { cardKeys: readerCardKeys() })} onClick={openCardLink} />}</Show>

      <Show when={!props.inThread && q().thread}>
        {(t) => (
          <div class="small">
            In a thread with the agent:{" "}
            <A class="btn small" href={threadHref(t())}>
              Open in thread
            </A>
          </div>
        )}
      </Show>

      <Show when={q().status === "open" || q().status === "in_thread"}>
        <form class="stack" style={{ gap: "6px" }} onSubmit={send} aria-label={notice() ? "Acknowledge" : "Answer"}>
          <ErrorBanner error={error()} onDismiss={() => setError(null)} />
          <Show when={!notice() && q().options.length}>
            <div class="row" style={{ "flex-wrap": "wrap", gap: "6px" }} role="group" aria-label="Options">
              <For each={q().options}>
                {(o, i) => (
                  <Show when={q().multi_select} fallback={
                  <button type="button" class={option() === i() ? "primary" : ""} aria-pressed={option() === i()} onClick={() => { setSkip(false); setOption(option() === i() ? null : i()); }}>
                    {o}
                  </button>
                  }>
                    <label><input type="checkbox" checked={selected().includes(i())} onChange={(e) => { setSkip(false); setSelected(e.currentTarget.checked ? [...selected(), i()] : selected().filter((n) => n !== i())); }} />{o}</label>
                  </Show>
                )}
              </For>
            </div>
          </Show>
          <Show when={!notice()}>
            <button type="button" aria-pressed={skip()} onClick={() => { setSkip(!skip()); setOption(null); }}>Skip question</button>
          </Show>
          <textarea
            rows={2}
            placeholder={skip() ? "Why are you skipping this question? (required)" : notice() ? "A reply to the agent (optional)" : q().options.length ? "An answer in your own words (optional with an option)" : "Your answer"}
            value={text()}
            onInput={(e) => setText(e.currentTarget.value)}
            aria-label={skip() ? "Skip reason" : notice() ? "Reply text" : "Answer text"}
          />
          <div class="row" style={{ "flex-wrap": "wrap", gap: "6px" }}>
            <Show
              when={notice()}
              fallback={
                <button class="primary" type="submit" disabled={busy() || (skip() ? !text().trim() : !q().multi_select && !text().trim() && option() === null)}>
                  {skip() ? "Send skip reason" : "Send answer"}
                </button>
              }
            >
              <button class="primary" type="submit" disabled={busy()}>
                Acknowledge
              </button>
            </Show>
            <input placeholder="Reason to retract (optional)" value={reason()} onInput={(e) => setReason(e.currentTarget.value)} aria-label="Retract reason" />
            <ConfirmButton class="small" label="Retract" confirmLabel="Yes, retract" disabled={busy()} onConfirm={() => run(() => api.questions.retract(q().id, reason().trim() || undefined))} />
            <Show when={!props.inThread && !q().thread && canOpenInThread(q())}>
              <input placeholder="Message for the thread (optional)" value={threadMessage()} maxLength={THREAD_MOVE_MESSAGE_MAX} onInput={(e) => setThreadMessage(e.currentTarget.value)} aria-label="Thread message" />
              <ConfirmButton
                class="small"
                label="Open in thread"
                confirmLabel="Yes, open a thread"
                disabled={busy()}
                onConfirm={async () => {
                  let opened: string | undefined;
                  await run(async () => {
                    const { question, thread } = await api.questions.toThread(q().id, threadMessage().trim() || undefined);
                    opened = thread.id;
                    return question;
                  });
                  if (opened) navigate(threadHref(opened));
                }}
              />
            </Show>
          </div>
        </form>
      </Show>

      <Show when={q().status === "answered" || q().status === "acknowledged"}>
        <For each={q().answer_options}>{(i) => <div>Chosen: <strong>{q().options[i]}</strong></div>}</For>
        <div class="stack small" style={{ gap: "4px" }}>
          <Show when={chosenLabel(q())}>
            {(l) => (
              <div>
                Chosen: <strong>{l()}</strong>
              </div>
            )}
          </Show>
          <Show when={q().answer && q().answer !== chosenLabel(q())}>
            <div style={{ "white-space": "pre-wrap" }}>{q().answer}</div>
          </Show>
          <div class="muted" title={q().answered_at ?? ""}>
            {q().status} by {q().answered_by_name ?? q().answered_by ?? "operator"} {q().answered_at ? ago(q().answered_at!, props.now) : ""}
            <Show when={q().status === "acknowledged" && !q().answer}> with no reply</Show>
          </div>
        </div>
      </Show>

      <Show when={q().status === "retracted"}>
        <div class="small muted">
          retracted by {q().retracted_by === "operator" ? "the operator" : q().retracted_by?.startsWith("agent:") ? "the agent" : (q().retracted_by ?? "—")}
          <Show when={q().retract_reason}>{(r) => <span style={{ "white-space": "pre-wrap" }}>: {r()}</span>}</Show>
        </div>
      </Show>
    </article>
  );
}
