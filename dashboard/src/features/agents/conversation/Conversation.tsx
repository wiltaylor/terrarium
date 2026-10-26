// The Conversation tab of the agent page: the harness transcript as blocks
// (docs/api.md §6, transcripts). The raw terminal stays on its own tab.
//
// Agent-written text is untrusted. Assistant markdown goes through
// `assistantHtml` and the `Markdown` component (DOMPurify via setSafeHtml).
// User turns, tool input, and tool output render as text nodes only.

import { createEffect, createMemo, createSignal, For, Match, on, onCleanup, Show, Switch } from "solid-js";
import type { Agent, Block, Session } from "../../../api/types";
import { createPagedList } from "../../../components/paged";
import { Markdown } from "../../../components/Markdown";
import { StatusBadge } from "../../../components/StatusBadge";
import { ErrorBanner, LoadMore } from "../../../components/ui";
import { ago } from "../../../lib/format";
import { api } from "../../../stores";
import {
  assistantHtml,
  composerState,
  footerLine,
  groupBlocks,
  isLongOutput,
  isSendKey,
  outputPreview,
  prettyInput,
  TAIL_START,
  tailOnAppend,
  tailOnScroll,
  type RenderItem,
} from "./model";
import { createTranscript, type Change } from "./transcript";

const LIVE = ["starting", "idle", "working", "blocked", "restarting"];

const timeOf = (at: string | null | undefined) => (at ? new Date(at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) : "");
const sessionLabel = (s: Session) => s.title?.trim() || s.harness_session_id || s.id;

export function Conversation(props: { agent: Agent; onOpenTerminal: () => void }) {
  // ----- sessions -----
  const sessions = createPagedList(
    () => props.agent.id,
    (aid, cursor) => api.agents.sessions(aid, { cursor }),
    (s) => s.id,
  );
  // A new session, or a start or a stop, changes the list and `active`.
  createEffect(
    on(
      () => [props.agent.session, LIVE.includes(props.agent.status)] as const,
      () => void sessions.reload(),
      { defer: true },
    ),
  );
  const [picked, setPicked] = createSignal<string | null>(null);
  const selected = createMemo<Session | undefined>(() => {
    const list = sessions.items();
    const p = picked();
    return (p ? list.find((s) => s.id === p) : undefined) ?? list.find((s) => s.active) ?? list.find((s) => s.current) ?? list[0];
  });

  // ----- scrolling -----
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
    agentId: () => props.agent.id,
    session: selected,
    fetch: (aid, sid, after, limit) => api.agents.transcript(aid, sid, { after, limit }),
    stream: (aid, sid, after) => api.agents.transcriptStream(aid, sid, { after }),
    onChange,
  });

  const loadEarlier = () => {
    if (scroller) keepFromBottom = scroller.scrollHeight - scroller.scrollTop;
    void t.loadEarlier();
  };

  const onScroll = () => {
    if (scroller) setTail((s) => tailOnScroll(s, scroller!));
  };
  const jump = () => {
    setTail(TAIL_START);
    toBottom();
  };

  // ----- render items, keyed so that a grown block keeps its DOM (and an open <details>) -----
  const items = createMemo(() => groupBlocks(t.blocks()));
  const byKey = createMemo(() => new Map(items().map((i) => [i.key, i])));
  const keys = createMemo(() => items().map((i) => i.key), [], { equals: (a, b) => a.length === b.length && a.every((k, i) => k === b[i]) });

  // The transcript's own `active` (its page, then `status` frames). When it cannot load, the list's word.
  const isActive = () => {
    if (!t.error()) return t.active();
    const s = selected();
    return s?.active ?? (!!s?.current && LIVE.includes(props.agent.status));
  };

  return (
    <div class="convo">
      <aside class="convo-sessions" aria-label="Sessions">
        <div class="spread">
          <strong class="small">Sessions</strong>
          <Show when={sessions.loading()}>
            <span class="muted small">Loading…</span>
          </Show>
        </div>
        <ErrorBanner error={sessions.error()} />
        <ul>
          <For each={sessions.items()}>
            {(s) => (
              <li>
                <button
                  type="button"
                  class="convo-session"
                  classList={{ active: selected()?.id === s.id }}
                  aria-current={selected()?.id === s.id ? "true" : undefined}
                  onClick={() => setPicked(s.id)}
                  title={s.harness_session_id ?? s.id}
                >
                  <span class="convo-session-title">{sessionLabel(s)}</span>
                  <span class="row small muted" style={{ gap: "6px" }}>
                    <span>{ago(s.started_at)}</span>
                    <Show when={s.blocks !== undefined}>
                      <span>· {s.blocks} blocks</span>
                    </Show>
                    <Show when={s.active}>
                      <span class="chip ok">active</span>
                    </Show>
                    <Show when={!s.active && s.outcome}>
                      <span class="chip">{s.outcome}</span>
                    </Show>
                    <Show when={s.transcript === false}>
                      <span class="chip warn">no transcript</span>
                    </Show>
                  </span>
                </button>
              </li>
            )}
          </For>
        </ul>
        <LoadMore hasMore={sessions.hasMore()} loading={sessions.loading()} onMore={sessions.loadMore} />
        <Show when={!sessions.loading() && !sessions.initial() && sessions.items().length === 0}>
          <p class="muted small">No sessions yet. Start the agent.</p>
        </Show>
      </aside>

      <section class="convo-main" aria-label="Conversation">
        <div class="convo-strip">
          <span class="row" style={{ gap: "6px" }}>
            <StatusBadge status={props.agent.status} />
            <span class="muted small">
              {props.agent.harness}
              {props.agent.model ? ` · ${props.agent.model}` : ""}
            </span>
            <StreamChip state={t.streamState()} ended={!t.active()} />
          </span>
          <button type="button" class="link small" onClick={() => props.onOpenTerminal()}>
            Open terminal
          </button>
        </div>
        <ErrorBanner error={t.error()} />
        <ErrorBanner error={t.streamError()} />

        <div class="convo-scroll-wrap">
          <div class="convo-scroll" ref={scroller} onScroll={onScroll} role="log" aria-live="polite" aria-busy={t.loading()}>
            <Show when={t.hasEarlier()}>
              <div class="convo-earlier">
                <button type="button" class="small" disabled={t.loadingEarlier()} onClick={loadEarlier}>
                  {t.loadingEarlier() ? "Loading…" : "Load earlier"}
                </button>
              </div>
            </Show>
            <Show when={!t.loading() || t.blocks().length} fallback={<p class="muted">Loading the transcript…</p>}>
              <Show when={t.blocks().length || !selected()} fallback={<p class="muted convo-note">This session has no blocks yet.</p>}>
                <For each={keys()}>{(key) => <ItemView item={byKey().get(key)!} project={props.agent.project} active={t.active()} />}</For>
              </Show>
            </Show>
            <Show when={!selected() && !sessions.loading()}>
              <p class="muted convo-note">Pick a session.</p>
            </Show>
          </div>
          <Show when={tail().unseen > 0}>
            <button type="button" class="convo-pill" onClick={jump}>
              {tail().unseen} new {tail().unseen === 1 ? "message" : "messages"} ↓
            </button>
          </Show>
        </div>

        <Composer agent={props.agent} sessionActive={isActive()} />
      </section>
    </div>
  );
}

function StreamChip(props: { state: string; ended: boolean }) {
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

function ItemView(props: { item: RenderItem; project: string; active: boolean }) {
  const it = () => props.item;
  return (
    <Switch>
      <Match when={it().type === "user" && (it() as Extract<RenderItem, { type: "user" }>)}>
        {(u) => (
          <div class="convo-user">
            <div class="convo-bubble">
              <div class="convo-meta">
                <Show when={u().chip}>{(c) => <span class={`chip ${c().tone}`} title={c().title}>{c().label}</span>}</Show>
                <span class="muted small" title={u().block.at ?? ""}>{timeOf(u().block.at)}</span>
              </div>
              <div class="convo-text">{u().block.text}</div>
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

function Composer(props: { agent: Agent; sessionActive: boolean }) {
  const [text, setText] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const [sent, setSent] = createSignal(false);
  let timer: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(timer));
  const state = () => composerState({ agentStatus: props.agent.status, sessionActive: props.sessionActive });

  const send = async () => {
    const body = text().trim();
    if (!body || busy() || !state().enabled) return;
    setBusy(true);
    try {
      await api.agents.send(props.agent.id, body);
      setText("");
      setError(null);
      setSent(true);
      clearTimeout(timer);
      timer = setTimeout(() => setSent(false), 6000);
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <form
      class="convo-composer"
      onSubmit={(e) => {
        e.preventDefault();
        void send();
      }}
    >
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <div class="convo-compose-row">
        <textarea
          rows={2}
          value={text()}
          disabled={!state().enabled}
          placeholder={state().enabled ? "Type a turn. Enter sends, Shift+Enter adds a line." : (state().reason ?? "")}
          aria-label="Turn text"
          onInput={(e) => setText(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (isSendKey(e)) {
              e.preventDefault();
              void send();
            }
          }}
        />
        <button class="primary" type="submit" disabled={!state().enabled || busy() || !text().trim()}>
          {busy() ? "Sending…" : "Send"}
        </button>
      </div>
      <Show when={!state().enabled} fallback={
        <Show when={state().note || sent()}>
          <p class="small muted convo-compose-note">
            {state().note ?? ""}
            {state().note && sent() ? " · " : ""}
            {sent() ? "Sent. The turn shows here when the harness writes it." : ""}
          </p>
        </Show>
      }>
        <p class="small muted convo-compose-note">{state().reason}</p>
      </Show>
    </form>
  );
}
