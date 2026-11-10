// One bundle of related questions of a leader to the operator (kind
// `bundle`). The operator answers every item, then sends all answers in one
// step: "Submit all answers" is on only when each item has an option or a
// text. The draft is in the component and in sessionStorage for each bundle
// and revision (./bundle.ts). The texts and the options are the agent's
// words: they go into the page as text nodes only. The contexts are
// Markdown, and go through the DOMPurify sink (SafeHtml) only.

import { A } from "@solidjs/router";
import { createEffect, createSignal, For, type JSX, on, Show } from "solid-js";
import { ApiError } from "../../api/http";
import type { Question } from "../../api/types";
import { LinkText, openCardLink, readerCardKeys } from "../../components/LinkText";
import { SafeHtml } from "../../components/SafeHtml";
import { ConfirmButton, ErrorBanner } from "../../components/ui";
import { ago } from "../../lib/format";
import { renderMarkdown } from "../../lib/markdown";
import { api } from "../../stores";
import {
  andList,
  type BundleDraft,
  bundleAnswerBody,
  carryDraft,
  chosenItemLabel,
  clearDraft,
  emptyDraft,
  isComplete,
  itemDone,
  itemsOf,
  loadDraft,
  progressText,
  saveDraft,
  sessionDraftStorage,
} from "./bundle";
import { cardHref, statusChip } from "./questions";

export function BundleCard(props: { question: Question; now: number; showAgent?: boolean; showKind?: boolean; hideCard?: boolean; actions?: JSX.Element; onChange: (q: Question) => void }) {
  const q = () => props.question;
  const store = sessionDraftStorage();
  const [draft, setDraft] = createSignal<BundleDraft>(loadDraft(store, q()) ?? emptyDraft(q()));
  const [reason, setReason] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  /** Set when the agent changed the bundle while the operator answered it. */
  const [changedNote, setChangedNote] = createSignal<string | null>(null);

  // A new revision: keep the answers of the items that did not change.
  let seen: Question = q();
  createEffect(
    on(
      () => [q().id, q().revision] as const,
      ([id, revision]) => {
        const before = seen;
        seen = q();
        if (before.id !== id) return void setDraft(loadDraft(store, q()) ?? emptyDraft(q()));
        if (before.revision === revision || q().status !== "open") return;
        const stored = loadDraft(store, q());
        const carried = carryDraft(before, draft(), q());
        clearDraft(store, before);
        setDraft(stored ?? carried.draft);
        saveDraft(store, q(), stored ?? carried.draft);
        const lost = stored ? [] : carried.changed;
        setChangedNote(
          `The agent changed this bundle (now revision ${revision}).` +
            (lost.length ? ` Check ${lost.length === 1 ? "item" : "items"} ${andList(lost)}: the answers there were cleared.` : " Your answers still fit."),
        );
      },
      { defer: true },
    ),
  );

  const edit = (i: number, patch: Partial<BundleDraft[number]>) => {
    const next = draft().map((d, j) => (j === i ? { ...d, ...patch } : d));
    setDraft(next);
    saveDraft(store, q(), next);
  };

  const run = async (call: () => Promise<Question>) => {
    setBusy(true);
    try {
      const before = q();
      const done = await call();
      clearDraft(store, before);
      setChangedNote(null);
      props.onChange(done);
      setError(null);
    } catch (e) {
      setError(e);
      // A conflict: the agent changed the bundle. Read it again, so the form shows the new items.
      if (e instanceof ApiError && e.status === 409) {
        try {
          props.onChange(await api.questions.get(q().id));
        } catch {
          /* the banner shows the first error */
        }
      }
    } finally {
      setBusy(false);
    }
  };
  const send = (e: Event) => {
    e.preventDefault();
    const b = bundleAnswerBody(q(), draft());
    if ("error" in b) return setError(b.error);
    void run(() => api.questions.answer(q().id, b.body));
  };
  const open = () => q().status === "open";
  const complete = () => isComplete(q(), draft());

  return (
    <article
      class={`card-box stack question bundle${q().priority !== "normal" && open() ? " urgent" : ""}${q().priority === "critical" && open() ? " critical" : ""}`}
      style={{ gap: "8px" }}
      aria-label={`Question bundle ${q().id}`}
    >
      <div class="row small" style={{ "flex-wrap": "wrap", gap: "6px" }}>
        <Show when={props.showKind}>
          <span class="chip">bundle of {itemsOf(q()).length}</span>
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
          asked {ago(q().created_at, props.now)}
        </span>
        <Show when={q().revision > 1}>
          <span class="muted" title={`Updated ${q().updated_at}`}>
            revision {q().revision}
          </span>
        </Show>
        <span class="mono muted">{q().id}</span>
        {props.actions}
      </div>

      <p class="question-text" style={{ margin: 0, "white-space": "pre-wrap", "font-weight": 600 }}>
        <LinkText text={q().text} project={q().project} />
      </p>

      <Show when={q().context}>{(c) => <SafeHtml class="prose question-context" html={renderMarkdown(c(), q().project, { cardKeys: readerCardKeys() })} onClick={openCardLink} />}</Show>

      <Show when={open()}>
        <form class="stack" style={{ gap: "10px" }} onSubmit={send} aria-label="Answer all items">
          <Show when={changedNote()}>
            {(n) => (
              <div class="notice small" role="status">
                {n()}
              </div>
            )}
          </Show>
          <ol class="bundle-items stack">
            <For each={itemsOf(q())}>
              {(item, i) => {
                const d = () => draft()[i()] ?? { option: null, text: "" };
                const label = () => `Item ${i() + 1}`;
                return (
                  <li class="bundle-item stack" classList={{ done: itemDone(d()) }} style={{ gap: "6px" }}>
                    <fieldset class="stack" style={{ gap: "6px" }}>
                      <legend class="bundle-legend">
                        <span class="bundle-num" aria-hidden="true">
                          {i() + 1}.
                        </span>{" "}
                        <span style={{ "white-space": "pre-wrap" }}><LinkText text={item.text} project={q().project} /></span>
                        <Show when={itemDone(d())}>
                          <span class="chip ok small bundle-check">answered</span>
                        </Show>
                      </legend>
                      <Show when={item.context}>{(c) => <SafeHtml class="prose question-context small" html={renderMarkdown(c(), q().project, { cardKeys: readerCardKeys() })} onClick={openCardLink} />}</Show>
                      <Show when={item.options.length}>
                        <div class="row" style={{ "flex-wrap": "wrap", gap: "6px" }} role="group" aria-label={`${label()} options`}>
                          <For each={item.options}>
                            {(o, k) => (
                              <button type="button" class={d().option === k() ? "primary" : ""} aria-pressed={d().option === k()} onClick={() => edit(i(), { option: d().option === k() ? null : k(), skip: false })}>
                                {o}
                              </button>
                            )}
                          </For>
                        </div>
                      </Show>
                      <button type="button" aria-pressed={!!d().skip} onClick={() => edit(i(), { skip: !d().skip, option: null })}>Skip question</button>
                      <textarea
                        rows={2}
                        placeholder={d().skip ? "Why are you skipping this question? (required)" : item.options.length ? "An answer in your own words (optional with an option)" : "Your answer"}
                        value={d().text}
                        onInput={(e) => edit(i(), { text: e.currentTarget.value })}
                        aria-label={`${label()} ${d().skip ? "skip reason" : "answer text"}`}
                      />
                    </fieldset>
                  </li>
                );
              }}
            </For>
          </ol>
          <ErrorBanner error={error()} onDismiss={() => setError(null)} />
          <div class="row" style={{ "flex-wrap": "wrap", gap: "6px" }}>
            <button class="primary" type="submit" disabled={busy() || !complete()} title={complete() ? "Send every answer to the agent in one mail" : "Answer every item first"}>
              Submit all answers
            </button>
            <span class="small" classList={{ muted: !complete() }} aria-live="polite">
              {progressText(q(), draft())}
            </span>
            <input placeholder="Reason to retract (optional)" value={reason()} onInput={(e) => setReason(e.currentTarget.value)} aria-label="Retract reason" />
            <ConfirmButton class="small" label="Retract" confirmLabel="Yes, retract all" disabled={busy()} onConfirm={() => run(() => api.questions.retract(q().id, reason().trim() || undefined))} />
          </div>
        </form>
      </Show>

      <Show when={q().status === "answered"}>
        <ol class="bundle-items stack small">
          <For each={itemsOf(q())}>
            {(item, i) => (
              <li class="bundle-item stack" style={{ gap: "2px" }}>
                <div>
                  <span class="bundle-num">{i() + 1}.</span> <span style={{ "white-space": "pre-wrap" }}><LinkText text={item.text} project={q().project} /></span>
                </div>
                <Show when={chosenItemLabel(item)}>
                  {(l) => (
                    <div>
                      Chosen: <strong>{l()}</strong>
                    </div>
                  )}
                </Show>
                <Show when={item.answer && item.answer !== chosenItemLabel(item)}>
                  <div style={{ "white-space": "pre-wrap" }}>{item.answer}</div>
                </Show>
              </li>
            )}
          </For>
        </ol>
        <div class="small muted" title={q().answered_at ?? ""}>
          answered by {q().answered_by_name ?? q().answered_by ?? "operator"} {q().answered_at ? ago(q().answered_at!, props.now) : ""}
        </div>
      </Show>

      <Show when={q().status === "retracted"}>
        <ol class="bundle-items stack small muted">
          <For each={itemsOf(q())}>{(item) => <li style={{ "white-space": "pre-wrap" }}>{item.text}</li>}</For>
        </ol>
        <div class="small muted">
          retracted by {q().retracted_by === "operator" ? "the operator" : q().retracted_by?.startsWith("agent:") ? "the agent" : (q().retracted_by ?? "—")}
          <Show when={q().retract_reason}>{(r) => <span style={{ "white-space": "pre-wrap" }}>: {r()}</span>}</Show>
        </div>
      </Show>
    </article>
  );
}
