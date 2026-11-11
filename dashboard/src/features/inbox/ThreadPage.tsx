import { useInstance } from "../../instance";
// The chat page of one Inbox thread (docs/api.md §6.13): the messages and
// the questions of the agent in order, with the questions answerable inline;
// a message box; the "Send response" box (the overall response, the approval
// record); the agent's close proposal with Confirm and Decline (TER-95); and
// the close with a confirmation and a handoff. The linked cards (TER-147)
// are a mini kanban under the title: open while the thread tracks.
//
// Every text of the thread is untrusted: the messages are Markdown and go
// through the DOMPurify sink (SafeHtml) only; the topic, the names, the
// handoff summary, and the link refs are text nodes.

import { useParams } from "@solidjs/router";
import { A } from "../../instance/navigation";
import { createEffect, createSignal, For, Match, on, onCleanup, onMount, Show, Switch } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import { ApiError } from "../../api/http";
import type { Question, Thread } from "../../api/types";
import { SafeHtml } from "../../components/SafeHtml";
import { Empty, ErrorBanner, Modal, PageHead, useNow } from "../../components/ui";
import { ago } from "../../lib/format";
import { renderMarkdown } from "../../lib/markdown";

import { BundleCard } from "./BundleCard";
import { CloseProposal, HandoffLinks } from "./CloseProposal";
import { QuestionCard } from "./QuestionCard";
import { TrackedCards } from "./TrackedCards";
import { ThreadIcon } from "./ThreadsPage";
import { byOperator, closeBody, retractedOnClose, scopeLabel, THREAD_MESSAGE_MAX, THREAD_RESPONSE_MAX, threadChip, threadText, timeline, type TimelineEntry, whoName } from "./threads";

export default function ThreadPage() {
  const { api, events, inbox } = useInstance();
  const params = useParams<{ id: string }>();
  const now = useNow(30_000);
  const [thread, setThread] = createSignal<Thread | null>(null);
  const [loading, setLoading] = createSignal(true);
  const [error, setError] = createSignal<unknown>(null);
  let gen = 0;
  const load = async () => {
    const mine = ++gen;
    try {
      const t = await api.threads.get(params.id);
      if (mine !== gen) return;
      setThread(t);
      setError(null);
    } catch (e) {
      if (mine === gen) setError(e);
    } finally {
      if (mine === gen) setLoading(false);
    }
  };
  createEffect(on(() => params.id, () => (setLoading(true), setThread(null), void load())));
  // The conversation in a store, reconciled by `id`: a reload keeps each
  // entry, and so the DOM and the local state of each question card that
  // stays (a chosen option, a typed answer, the focus).
  const [log, setLog] = createStore<{ entries: TimelineEntry[] }>({ entries: [] });
  createEffect(() => {
    const t = thread();
    setLog("entries", reconcile(t ? timeline(t) : [], { key: "id", merge: true }));
  });

  onMount(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      const d = (e.data ?? {}) as Record<string, unknown>;
      const mineThread = e.kind === "thread" && d.id === params.id;
      const mineQuestion = e.kind === "question" && !!thread()?.questions?.some((q) => q.id === d.question_id);
      if (!mineThread && !mineQuestion) return;
      clearTimeout(timer);
      timer = setTimeout(() => void load(), 300);
    });
    onCleanup(() => (off(), clearTimeout(timer)));
  });

  /** A write answered the thread: show it, and let the badge count again. */
  const changed = (t: Thread) => {
    setThread(t);
    void inbox.refresh();
  };
  /** An inline question changed: show it at once, then read the thread for its new status. */
  const questionChanged = (q: Question) => {
    setThread((t) => (t ? { ...t, questions: (t.questions ?? []).map((x) => (x.id === q.id ? q : x)) } : t));
    void load();
    void inbox.refresh();
  };
  const closed = () => thread()?.status === "closed";
  const project = () => {
    const s = thread()?.scope;
    return s && s !== "global" ? s : "";
  };
  const [closing, setClosing] = createSignal(false);
  const [reply, setReply] = createSignal("");
  const [quoting, setQuoting] = createSignal(0);
  const quote = (text: string, post: HTMLElement) => {
    const selection = window.getSelection();
    const selected = selection && post.contains(selection.anchorNode) && post.contains(selection.focusNode) ? selection.toString() : "";
    const value = (selected || text).split("\n").map((line) => `> ${line}`).join("\n");
    setReply((old) => `${old}${old ? "\n\n" : ""}${value}\n\n`);
    setQuoting((n) => n + 1);
  };
  /** A write got a 409: the thread changed elsewhere (closed, for example). Read it again, also without events. */
  const conflict = (e: unknown) => {
    if (e instanceof ApiError && e.status === 409) void load();
  };

  return (
    <div class="stack thread-page">
      <A href="/threads" class="small muted">
        ← Threads
      </A>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={thread()} fallback={<Empty>{loading() ? "Loading…" : "No such thread."}</Empty>}>
        {(t) => (
          <>
            <PageHead title={t().topic}>
              <Show when={!closed()}>
                <button type="button" class="danger" onClick={() => setClosing(true)}>
                  Close the thread
                </button>
              </Show>
            </PageHead>
            <ThreadMeta thread={t()} now={now()} />
            <TrackedCards thread={t()} now={now()} />
            <div class="stack thread-log" style={{ gap: "10px" }} aria-label="Conversation">
              <Show when={log.entries.length} fallback={<Empty>No messages yet.</Empty>}>
                <For each={log.entries}>
                  {(entry, index) => {
                    let post!: HTMLElement;
                    const author = () => entry.kind === "message" ? entry.message.author_name : entry.kind === "question" ? entry.question.agent_name : whoName(t(), entry.kind === "response" ? t().response?.by ?? null : entry.kind === "proposal" ? t().close_proposal?.by ?? null : t().closed_by);
                    const operator = () => entry.kind === "message" ? byOperator(entry.message) : entry.kind === "response" || (entry.kind === "handoff" && t().closed_by === "operator");
                    const label = () => entry.kind === "message" ? (entry.message.kind === "response" ? "Response" : "Message") : entry.kind === "question" ? (entry.question.kind === "bundle" ? "Question bundle" : "Question") : entry.kind === "proposal" ? "Close proposal" : entry.kind === "handoff" ? "Handoff" : "Response";
                    const text = () => entry.kind === "message" ? entry.message.text : entry.kind === "question" ? entry.question.text : entry.kind === "response" ? t().response?.text ?? "" : entry.kind === "proposal" ? t().close_proposal?.summary ?? "" : t().handoff?.summary ?? "";
                    return (
                    <article ref={post} id={`post-${index() + 1}`} class={`thread-post post-${label().toLowerCase().replaceAll(" ", "-")}`}>
                      <header class="thread-post-header">
                        <ThreadIcon icon={operator() ? "you" : "agent"} />
                        <strong>{author()}</strong>
                        <span class="chip">{label()}</span>
                        <Show when={entry.kind === "question" && entry.question.status === "open"}><span class="chip warn">Waiting on you</span></Show>
                        <time class="muted" dateTime={entry.at} title={entry.at}>{ago(entry.at, now())}</time>
                        <A href={`/threads/${encodeURIComponent(t().id)}#post-${index() + 1}`} aria-label={`Permalink to post ${index() + 1}`}>#{index() + 1}</A>
                        <Show when={!closed()}><button type="button" onClick={() => quote(text(), post)}>Quote</button></Show>
                      </header>
                      <div class="thread-post-body">
                    <Switch>
                      <Match when={entry.kind === "message" && entry.message}>
                        {(m) => (
                          <article class={`card-box stack thread-msg${byOperator(m()) ? " mine" : ""}${m().kind === "response" ? " response" : ""}`} style={{ gap: "4px" }} aria-label={`Message ${m().id}`}>
                            <SafeHtml class="prose" html={renderMarkdown(m().text, project())} />
                          </article>
                        )}
                      </Match>
                      <Match when={entry.kind === "question" && entry.question}>
                        {(q) => (
                          <div class="thread-question">
                            <Show when={q().kind === "bundle"} fallback={<QuestionCard question={q()} now={now()} showKind inThread onChange={questionChanged} />}>
                              <BundleCard question={q()} now={now()} showKind onChange={questionChanged} />
                            </Show>
                          </div>
                        )}
                      </Match>
                      <Match when={entry.kind === "response"}><section aria-label="Your response"><SafeHtml class="prose" html={renderMarkdown(text(), project())} /></section></Match>
                      <Match when={entry.kind === "proposal"}><CloseProposal thread={t()} now={now()} onChanged={changed} onError={conflict} /></Match>
                      <Match when={entry.kind === "handoff"}><Handoff thread={t()} now={now()} /></Match>
                    </Switch>
                      </div>
                    </article>
                  ); }}
                </For>
              </Show>
            </div>
            <Show when={!closed()}>
              <Composer
                label="Message"
                placeholder={t().agent_name ? `A message to ${t().agent_name} (Markdown)` : "A message (Markdown)"}
                button="Send"
                max={THREAD_MESSAGE_MAX}
                text={reply()}
                onText={setReply}
                quoting={quoting()}
                project={project()}
                send={(text) => api.threads.post(t().id, text)}
                onSent={changed}
                onError={conflict}
              />
              <Composer
                label="Response"
                title="Send response"
                hint="Your overall response to the agent: the approval record. It replaces the last one; the agent quotes it word for word."
                placeholder="Your overall response (Markdown)"
                button="Send response"
                max={THREAD_RESPONSE_MAX}
                project={project()}
                send={(text) => api.threads.respond(t().id, text)}
                onSent={changed}
                onError={conflict}
              />
            </Show>
            <Show when={closing()}>
              <CloseDialog
                thread={t()}
                onClose={() => setClosing(false)}
                onError={conflict}
                onClosed={(next) => {
                  setClosing(false);
                  changed(next);
                }}
              />
            </Show>
          </>
        )}
      </Show>
    </div>
  );
}

/** The line under the title: status, scope, agent, report-to lead, template, opener. */
function ThreadMeta(props: { thread: Thread; now: number }) {
  const t = () => props.thread;
  const chip = () => threadChip(t());
  return (
    <div class="row small" style={{ gap: "8px", "flex-wrap": "wrap" }} aria-label="Thread details">
      <span class={`chip ${chip().chip}`} data-testid="thread-status">
        {chip().label}
      </span>
      <span class="muted">{scopeLabel(t().scope)}</span>
      <Show when={t().agent} fallback={<span class="muted">no agent attached</span>}>
        {(id) => (
          <span>
            agent <A href={`/agents/${encodeURIComponent(id())}`}>{t().agent_name ?? id()}</A>
          </span>
        )}
      </Show>
      <Show when={t().report_to_name}>{(n) => <span>reports to {n()}</span>}</Show>
      <Show when={t().agent_end_outcome}>{(outcome) => <span class="warn">agent ended: {outcome()}. Nobody will answer in this thread.</span>}</Show>
      <Show when={t().template}>{(n) => <span class="muted">template {n()}</span>}</Show>
      <span class="muted" title={t().created_at}>
        opened by {t().opened_by_name} {ago(t().created_at, props.now)}
      </span>
    </div>
  );
}

/** A text box with one send button: a message or the overall response. */
function Composer(props: { label: string; title?: string; hint?: string; placeholder: string; button: string; max: number; text?: string; onText?: (text: string) => void; quoting?: number; project: string; send: (text: string) => Promise<Thread>; onSent: (t: Thread) => void; onError?: (e: unknown) => void }) {
  const [localText, setLocalText] = createSignal("");
  const text = () => props.text ?? localText();
  const setText = (value: string) => props.onText ? props.onText(value) : setLocalText(value);
  const [preview, setPreview] = createSignal(false);
  let editor!: HTMLTextAreaElement;
  createEffect(on(() => props.quoting, (n) => { if (n) { setPreview(false); queueMicrotask(() => { editor.focus(); editor.setSelectionRange(text().length, text().length); }); } }));
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const submit = async (e: Event) => {
    e.preventDefault();
    const what = props.label.toLowerCase();
    const t = threadText(text(), props.max, what);
    if ("error" in t) return setError(t.error);
    setBusy(true);
    try {
      props.onSent(await props.send(t.text));
      setText("");
      setError(null);
    } catch (err) {
      setError(err);
      props.onError?.(err);
    } finally {
      setBusy(false);
    }
  };
  return (
    <form class="card-box stack" style={{ gap: "6px" }} onSubmit={submit} aria-label={props.title ?? props.label}>
      <Show when={props.title}>
        <strong class="small">{props.title}</strong>
      </Show>
      <Show when={props.hint}>
        <span class="small muted">{props.hint}</span>
      </Show>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <div role="tablist" aria-label={`${props.label} editor`} class="row">
        <button type="button" role="tab" aria-selected={!preview()} onClick={() => setPreview(false)}>Write</button>
        <button type="button" role="tab" aria-selected={preview()} onClick={() => setPreview(true)}>Preview</button>
      </div>
      <div role="tabpanel" hidden={preview()}><textarea ref={editor} rows={5} placeholder={props.placeholder} value={text()} onInput={(e) => setText(e.currentTarget.value)} aria-label={`${props.label} text`} /></div>
      <Show when={preview()}><div role="tabpanel" aria-label={`${props.label} preview`}><SafeHtml class="prose" html={renderMarkdown(text(), props.project)} /></div></Show>
      <div>
        <button class="primary" type="submit" disabled={busy()}>
          {props.button}
        </button>
      </div>
    </form>
  );
}

/** The confirmation of a close, with the handoff: a summary and links. The open questions of the thread are retracted. */
function CloseDialog(props: { thread: Thread; onClose: () => void; onClosed: (t: Thread) => void; onError?: (e: unknown) => void }) {
  const { api } = useInstance();
  const [summary, setSummary] = createSignal("");
  const [links, setLinks] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const open = () => retractedOnClose(props.thread);
  const submit = async (e: Event) => {
    e.preventDefault();
    const b = closeBody(summary(), links());
    if ("error" in b) return setError(b.error);
    setBusy(true);
    try {
      props.onClosed(await api.threads.close(props.thread.id, b.body));
    } catch (err) {
      setError(err);
      props.onError?.(err);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal title="Close the thread" onClose={() => props.onClose()}>
      <form class="stack" style={{ gap: "8px" }} onSubmit={submit} aria-label="Close the thread">
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <p class="small" style={{ margin: 0 }}>
          A closed thread takes no more messages. It stays readable under Threads, on the Done tab.
        </p>
        <Show when={open() > 0}>
          <p class="small warn-text" style={{ margin: 0 }} data-testid="close-retracts">
            {open()} open {open() === 1 ? "question is" : "questions are"} retracted, and the agent gets a mail for each.
          </p>
        </Show>
        <label class="stack small" style={{ gap: "2px" }}>
          Handoff summary (optional)
          <textarea rows={3} value={summary()} onInput={(e) => setSummary(e.currentTarget.value)} aria-label="Summary" />
        </label>
        <label class="stack small" style={{ gap: "2px" }}>
          Links (optional): one per line, as <code>card TER-12</code>, <code>wiki page</code>, or a URL
          <textarea rows={2} value={links()} onInput={(e) => setLinks(e.currentTarget.value)} aria-label="Links" />
        </label>
        <div class="row" style={{ gap: "6px" }}>
          <button class="danger" type="submit" disabled={busy()}>
            Yes, close the thread
          </button>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}

/** The handoff of a closed thread. */
function Handoff(props: { thread: Thread; now: number }) {
  const t = () => props.thread;
  return (
    <section class="card-box stack" style={{ gap: "6px" }} aria-label="Handoff">
      <span class="small muted">
        <Show when={t().close_reason === "confirmed"} fallback={<>Closed by {whoName(t(), t().closed_by)}</>}>
          You confirmed the close that {whoName(t(), t().handoff?.by ?? null)} proposed
        </Show>{" "}
        {ago(t().closed_at ?? undefined, props.now)}.
      </span>
      <Show when={t().handoff?.summary}>{(s) => <p style={{ margin: 0, "white-space": "pre-wrap" }}>{s()}</p>}</Show>
      <HandoffLinks links={t().handoff?.links ?? []} />
    </section>
  );
}
