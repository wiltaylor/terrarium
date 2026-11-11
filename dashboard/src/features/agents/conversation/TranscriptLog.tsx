import { useInstance } from "../../../instance";
// The transcript of one session as blocks (docs/api.md §6, transcripts): the
// window of blocks, "load earlier", the live tail, and the scroll logic. The
// Conversation tab shows the current session with it; the Sessions tab shows
// any session, read-only.
//
// Agent-written text is untrusted. Assistant markdown goes through
// `assistantHtml` and the `Markdown` component (DOMPurify via setSafeHtml).
// User turns, tool input, and tool output render as text nodes only.

import { createMemo, createSignal, For, Match, Show, Switch, type JSX } from "solid-js";
import type { Agent, Block, Session, TurnPart } from "../../../api/types";
import { Markdown } from "../../../components/Markdown";
import { ErrorBanner } from "../../../components/ui";

import {
  assistantHtml,
  bodySegments,
  footerLine,
  groupBlocks,
  isLongOutput,
  outputPreview,
  partBadge,
  prettyInput,
  TAIL_START,
  tailOnAppend,
  tailOnScroll,
  type RenderItem,
} from "./model";
import { createTranscript, type Change, type TranscriptSource } from "./transcript";

const timeOf = (at: string | null | undefined) => (at ? new Date(at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) : "");

export type TranscriptState = ReturnType<typeof createTranscript>;

/**
 * The transcript of `session` with its scroll state. `TranscriptLog`
 * renders it; the caller reads `t` for the status (`active`, the stream).
 */
export function createTranscriptView(
  agent: () => Pick<Agent, "id" | "project">,
  session: () => Session | undefined,
  /** The page source. Default: the agent's own transcript endpoint. The session page passes `GET /api/sessions/{id}/transcript`. */
  fetch?: TranscriptSource["fetch"],
) {
  const { api } = useInstance();
  const fetchPage: TranscriptSource["fetch"] = fetch ?? ((aid, sid, after, limit) => api.agents.transcript(aid, sid, { after, limit }));
  let scroller: HTMLDivElement | undefined;
  const [tail, setTail] = createSignal(TAIL_START);
  const toBottom = () => {
    if (scroller) scroller.scrollTop = scroller.scrollHeight;
  };
  const afterRender = (fn: () => void) => requestAnimationFrame(() => fn());
  let keepFromBottom: number | null = null;

  const onChange = (c: Change) => {
    if (c.kind === "reset") {
      setTail(TAIL_START);
      afterRender(toBottom);
    } else if (c.kind === "prepend") {
      // Keep the view where it was: the same distance from the end.
      const from = keepFromBottom;
      keepFromBottom = null;
      afterRender(() => {
        if (scroller && from !== null) scroller.scrollTop = scroller.scrollHeight - from;
      });
    } else {
      setTail((t) => tailOnAppend(t, c.count));
      if (tail().follow) afterRender(toBottom);
    }
  };

  const t = createTranscript({
    agentId: () => agent().id,
    session,
    fetch: fetchPage,
    stream: (aid, sid, after) => api.agents.transcriptStream(aid, sid, { after }),
    onChange,
  });

  // Render items, keyed so that a grown block keeps its DOM (and an open <details>).
  const items = createMemo(() => groupBlocks(t.blocks()));
  const byKey = createMemo(() => new Map(items().map((i) => [i.key, i])));
  const keys = createMemo(() => items().map((i) => i.key), [], { equals: (a, b) => a.length === b.length && a.every((k, i) => k === b[i]) });

  return {
    t,
    session,
    agent,
    tail,
    keys,
    byKey,
    setScroller: (el: HTMLDivElement) => (scroller = el),
    onScroll: () => {
      if (scroller) setTail((s) => tailOnScroll(s, scroller!));
    },
    jump: () => {
      setTail(TAIL_START);
      toBottom();
    },
    loadEarlier: () => {
      if (scroller) keepFromBottom = scroller.scrollHeight - scroller.scrollTop;
      void t.loadEarlier();
    },
  };
}

export type TranscriptView = ReturnType<typeof createTranscriptView>;

/** The scroll area of a transcript: "load earlier", the blocks, the "new messages" pill. `empty` shows with no session. */
export function TranscriptLog(props: { view: TranscriptView; empty: JSX.Element; label?: string }) {
  const v = () => props.view;
  const t = () => props.view.t;
  return (
    <>
      <ErrorBanner error={t().error()} />
      <ErrorBanner error={t().streamError()} />
      <div class="convo-scroll-wrap">
        <div class="convo-scroll" ref={(el) => v().setScroller(el)} onScroll={() => v().onScroll()} role="log" aria-live="polite" aria-label={props.label} aria-busy={t().loading()}>
          <Show when={v().session()} fallback={props.empty}>
            <Show when={t().hasEarlier()}>
              <div class="convo-earlier">
                <button type="button" class="small" disabled={t().loadingEarlier()} onClick={() => v().loadEarlier()}>
                  {t().loadingEarlier() ? "Loading…" : "Load earlier"}
                </button>
              </div>
            </Show>
            <Show when={!t().loading() || t().blocks().length} fallback={<p class="muted">Loading the transcript…</p>}>
              <Show when={t().blocks().length} fallback={<p class="muted convo-note">This session has no blocks yet.</p>}>
                <For each={v().keys()}>{(key) => <ItemView item={v().byKey().get(key)!} project={v().agent().project} active={t().active()} />}</For>
              </Show>
            </Show>
          </Show>
        </div>
        <Show when={v().tail().unseen > 0}>
          <button type="button" class="convo-pill" onClick={() => v().jump()}>
            {v().tail().unseen} new {v().tail().unseen === 1 ? "message" : "messages"} ↓
          </button>
        </Show>
      </div>
    </>
  );
}

export function StreamChip(props: { state: string; ended: boolean }) {
  const view = () => {
    if (props.state === "open") return { label: "live", tone: "ok" };
    if (props.state === "connecting") return { label: "connecting…", tone: "" };
    if (props.state === "failed") return { label: "no live stream", tone: "bad" };
    if (props.state === "closed") return props.ended ? { label: "ended", tone: "" } : { label: "reconnecting…", tone: "warn" };
    return props.ended ? { label: "ended", tone: "" } : null;
  };
  return (
    <Show when={view()}>
      {(v) => <span class={`chip ${v().tone}`}>{v().label}</span>}
    </Show>
  );
}

/** One tagged part of a typed turn: the badge, then the body as plain text; a quoted mail body in a blockquote. */
function TurnPartView(props: { part: TurnPart }) {
  const badge = () => partBadge(props.part);
  return (
    <div class="convo-part">
      <span class={`chip ${badge().tone}`} title={badge().title}>
        {badge().label}
      </span>
      <For each={bodySegments(props.part.body)}>
        {(seg) => (seg.quote ? <blockquote class="convo-quote convo-text">{seg.text}</blockquote> : <div class="convo-text">{seg.text}</div>)}
      </For>
    </div>
  );
}

function ItemView(props: { item: RenderItem; project: string; active: boolean }) {
  const it = () => props.item;
  return (
    <Switch>
      <Match when={it().type === "user" && (it() as Extract<RenderItem, { type: "user" }>)}>
        {(u) => (
          <div class="convo-user">
            <div class="convo-bubble">
              <div class="convo-meta">
                <Show when={!u().parts.length && u().chip}>{(c) => <span class={`chip ${c().tone}`} title={c().title}>{c().label}</span>}</Show>
                <span class="muted small" title={u().block.at ?? ""}>{timeOf(u().block.at)}</span>
              </div>
              <Show when={u().parts.length} fallback={<div class="convo-text">{u().block.text}</div>}>
                <For each={u().parts}>{(p) => <TurnPartView part={p} />}</For>
              </Show>
            </div>
          </div>
        )}
      </Match>
      <Match when={it().type === "text" && (it() as Extract<RenderItem, { type: "text" }>)}>
        {(x) => (
          <div class="convo-assistant">
            <Markdown html={assistantHtml(x().block.text ?? "", props.project)} />
          </div>
        )}
      </Match>
      <Match when={it().type === "thinking" && (it() as Extract<RenderItem, { type: "thinking" }>)}>
        {(x) => <Thinking block={x().block} />}
      </Match>
      <Match when={it().type === "tool" && (it() as Extract<RenderItem, { type: "tool" }>)}>
        {(x) => <ToolCard call={x().call} result={x().result} active={props.active} />}
      </Match>
      <Match when={it().type === "result" && (it() as Extract<RenderItem, { type: "result" }>)}>
        {(x) => (
          <div class="convo-tool" classList={{ error: !!x().block.result?.is_error }}>
            <div class="convo-tool-head small muted">Result of an earlier tool call</div>
            <ToolOutput block={x().block} />
          </div>
        )}
      </Match>
      <Match when={it().type === "system" && (it() as Extract<RenderItem, { type: "system" }>)}>
        {(x) => (
          <div class="convo-system small muted" title={x().block.at ?? ""}>
            {x().block.text}
          </div>
        )}
      </Match>
      <Match when={it().type === "footer" && (it() as Extract<RenderItem, { type: "footer" }>)}>
        {(x) => <div class="convo-footer small muted">{footerLine(x().footer)}</div>}
      </Match>
    </Switch>
  );
}

function Thinking(props: { block: Block }) {
  const empty = () => props.block.redacted || !props.block.text?.trim();
  return (
    <Show
      when={!empty()}
      fallback={
        <div class="convo-thinking small muted" title="The harness did not keep the thinking text.">
          Thinking · redacted
        </div>
      }
    >
      <details class="convo-thinking small muted">
        <summary>Thinking…</summary>
        <div class="convo-text">{props.block.text}</div>
      </details>
    </Show>
  );
}

function ToolCard(props: { call: Block; result?: Block; active: boolean }) {
  const tool = () => props.call.tool;
  const failed = () => !!props.result?.result?.is_error;
  return (
    <div class="convo-tool" classList={{ error: failed() }}>
      <details>
        <summary class="convo-tool-head">
          <span class="mono convo-tool-name">{tool()?.name ?? "tool"}</span>
          <span class="convo-tool-summary ellipsis">{tool()?.summary}</span>
          <Show when={failed()}>
            <span class="chip bad">error</span>
          </Show>
          <Show when={!props.result && props.active}>
            <span class="chip">running…</span>
          </Show>
        </summary>
        <pre class="convo-pre">{prettyInput(tool()?.input)}</pre>
      </details>
      <Show when={props.result}>{(r) => <ToolOutput block={r()} />}</Show>
    </div>
  );
}

function ToolOutput(props: { block: Block }) {
  const r = () => props.block.result;
  const out = () => r()?.output ?? "";
  return (
    <div class="convo-output" classList={{ error: !!r()?.is_error }}>
      <Show
        when={isLongOutput(out())}
        fallback={
          <Show when={out()} fallback={<span class="muted small">(no output)</span>}>
            <pre class="convo-pre">{out()}</pre>
          </Show>
        }
      >
        <details>
          <summary class="small">
            Output · {out().split("\n").length} lines
            <pre class="convo-pre convo-preview">{outputPreview(out())}</pre>
          </summary>
          <pre class="convo-pre">{out()}</pre>
        </details>
      </Show>
      <Show when={r()?.truncated}>
        <p class="small muted convo-truncated">truncated: the harness wrote more than the transcript keeps</p>
      </Show>
    </div>
  );
}
