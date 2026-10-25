// The mail log (docs/api.md §7): every message across all projects, newest
// first. By default it shows only what needs the operator (the attention
// rule in ./log.ts). A row opens its thread inline, with reply and compose.

import { A, useSearchParams } from "@solidjs/router";
import { createEffect, createMemo, createSignal, For, on, onCleanup, Show } from "solid-js";
import { Empty, ErrorBanner, LoadMore, PageHead, useNow } from "../../components/ui";
import type { Message } from "../../api/types";
import { ago } from "../../lib/format";
import { api, attention, events, projects } from "../../stores";
import { createMailFeed, type MailFeed } from "./feed";
import {
  annotate,
  applyFilter,
  counterpart,
  filterFromParams,
  filterToParams,
  firstLine,
  isUnread,
  KIND_LABELS,
  needsAttention,
  RANGE_LABELS,
  serverQuery,
  threadOf,
  type LogEntry,
  type LogFilter,
  type LogParams,
  type MailKind,
  type TimeRange,
} from "./log";
import { Compose, MessageView, StateChips } from "./parts";

const PAGE = 50;

/** Mark the operator's unread messages read (`POST /api/mail/{id}/read`), so `read_by` is set. */
async function markOperatorRead(list: Message[]): Promise<Message[]> {
  const todo = list.filter((m) => m.to_kind === "operator" && isUnread(m));
  const done = await Promise.all(todo.map((m) => api.mail.markRead(m.id).catch(() => null)));
  const ok = done.filter((m): m is Message => !!m);
  attention.upsert(ok);
  return ok;
}

export default function MailPage() {
  const [params, setParams] = useSearchParams<LogParams>();
  const filter = createMemo(() => filterFromParams({ ...params }));
  const [limit, setLimit] = createSignal(PAGE);
  const setFilter = (patch: Partial<LogFilter>) => {
    setParams({ ...filterToParams({ ...filter(), ...patch }) }, { replace: true });
    setLimit(PAGE);
  };
  const now = useNow(60_000);
  const feed = createMailFeed(api, () => serverQuery(filter()));
  const entries = createMemo(() => annotate(feed.messages(), { prefixes: projects.prefixes(), live: attention.live() }));
  const byId = createMemo(() => new Map(entries().map((e) => [e.message.id, e])));
  const shown = createMemo(() => applyFilter(entries(), filter(), now()));
  const visible = () => shown().slice(0, limit());
  const [error, setError] = createSignal<unknown>(null);
  const [composing, setComposing] = createSignal<{ to?: string } | null>(null);
  const expanded = () => params.thread;
  /** One open row: the newest shown row of the open thread. */
  const openRow = createMemo(() => (expanded() ? visible().find((e) => e.message.thread_id === expanded())?.message.id : undefined));
  const toggle = (thread: string) => setParams({ thread: expanded() === thread ? undefined : thread });

  // Live: a `mail` event pulls the new messages and its thread again.
  let t: ReturnType<typeof setTimeout> | undefined;
  const touched = new Set<string>();
  const off = events.on((e) => {
    if (e.kind !== "mail") return;
    if (typeof e.data?.thread_id === "string") touched.add(e.data.thread_id);
    clearTimeout(t);
    t = setTimeout(() => {
      const threads = [...touched];
      touched.clear();
      void feed.pull(threads);
    }, 300);
  });
  onCleanup(() => (off(), clearTimeout(t)));

  // Opening a thread loads all of it (a filter can hide part of it), then
  // marks the operator's unread messages on it as read.
  createEffect(
    on(expanded, async (thread) => {
      if (!thread) return;
      await feed.pull([thread]);
      if (expanded() !== thread) return;
      feed.upsert(await markOperatorRead(threadOf(feed.messages(), thread)));
    }),
  );

  const markRead = async (m: Message) => {
    try {
      feed.upsert([await api.mail.markRead(m.id)]);
      attention.upsert(feed.messages().filter((x) => x.id === m.id));
    } catch (e) {
      setError(e);
    }
  };

  return (
    <>
      <PageHead title="Mail log">
        <button onClick={() => (void feed.reload(), void attention.refresh())}>Refresh</button>
        <button class="primary" onClick={() => setComposing({})}>
          Compose
        </button>
      </PageHead>

      <div class="row log-filters" role="search">
        <label class="row small attention-toggle" title="Unread mail to you, held mail, paused threads, blocked-card notices, quota alerts, and loop-guard notices">
          <input type="checkbox" checked={filter().attention} onChange={(e) => setFilter({ attention: e.currentTarget.checked })} /> Needs attention
          <Show when={attention.count() > 0}>
            <span class="nav-count info">{attention.count()}</span>
          </Show>
        </label>
        <select aria-label="Filter by project" value={filter().project} onChange={(e) => setFilter({ project: e.currentTarget.value })}>
          <option value="">All projects</option>
          <For each={projects.projects()}>
            {(p) => (
              <option value={p.name} selected={p.name === filter().project}>
                {p.name}
              </option>
            )}
          </For>
        </select>
        <select aria-label="Direction" value={filter().kind} onChange={(e) => setFilter({ kind: e.currentTarget.value as MailKind | "" })}>
          <option value="">Any direction</option>
          <For each={Object.entries(KIND_LABELS)}>
            {([k, label]) => (
              <option value={k} selected={k === filter().kind}>
                {label}
              </option>
            )}
          </For>
        </select>
        <select aria-label="Time range" value={filter().range} onChange={(e) => setFilter({ range: e.currentTarget.value as TimeRange })}>
          <For each={Object.entries(RANGE_LABELS)}>
            {([k, label]) => (
              <option value={k} selected={k === filter().range}>
                {label}
              </option>
            )}
          </For>
        </select>
        <input type="search" placeholder="Search subject, body, names" aria-label="Search" value={filter().q} onInput={(e) => setFilter({ q: e.currentTarget.value })} />
      </div>

      <ErrorBanner error={error() ?? feed.error()} onDismiss={() => setError(null)} />

      <div class="card-box mail-log" style={{ padding: "0" }}>
        <For
          each={visible()}
          fallback={
            <Empty>
              {!feed.loaded() ? (
                "Loading…"
              ) : filter().attention ? (
                <>
                  Nothing needs you.{" "}
                  <button class="small ghost" onClick={() => setFilter({ attention: false })}>
                    Show every message
                  </button>
                </>
              ) : (
                "No mail matches."
              )}
            </Empty>
          }
        >
          {(e) => (
            <LogRow
              entry={e}
              now={now()}
              open={openRow() === e.message.id}
              onToggle={() => toggle(e.message.thread_id)}
              onMarkRead={() => void markRead(e.message)}
              feed={feed}
              entryOf={(id) => byId().get(id)}
              onCompose={(to) => setComposing({ to })}
              onError={setError}
            />
          )}
        </For>
      </div>
      <LoadMore hasMore={shown().length > limit()} onMore={() => setLimit((n) => n + PAGE)} count={visible().length} />

      <Show when={composing()}>
        {(c) => (
          <Compose
            to={c().to}
            onClose={() => setComposing(null)}
            onSent={(m) => {
              feed.upsert([m]);
              void attention.refresh();
            }}
          />
        )}
      </Show>
    </>
  );
}

function LogRow(props: {
  entry: LogEntry;
  now: number;
  open: boolean;
  onToggle: () => void;
  onMarkRead: () => void;
  feed: MailFeed;
  entryOf: (id: string) => LogEntry | undefined;
  onCompose: (to?: string) => void;
  onError: (e: unknown) => void;
}) {
  const m = () => props.entry.message;
  const inbox = () => (m().to_kind === "project" ? m().to_name.slice(8) : undefined);
  return (
    <div class={`log-row${needsAttention(props.entry) ? " attn" : ""}${props.open ? " open" : ""}`}>
      <div class="log-head">
        <button class="log-main" onClick={() => props.onToggle()} aria-expanded={props.open}>
          <span class="log-meta small">
            <span class="muted" title={m().created_at}>
              {ago(m().created_at, props.now)}
            </span>
            <span>
              <strong>{m().from_name}</strong> → <strong>{m().to_name}</strong>
            </span>
            <Show when={props.entry.projects.length}>
              <span class="muted">{props.entry.projects.join(", ")}</span>
            </Show>
          </span>
          <span class="log-subject">
            <span class={isUnread(m()) ? "subject strong" : "subject"}>{m().subject || "(no subject)"}</span>
            <span class="muted"> {firstLine(m().body)}</span>
          </span>
        </button>
        <div class="log-side">
          <StateChips message={m()} held={props.entry.held} notice={props.entry.notice} reasons={props.entry.reasons} />
          <Show when={inbox()}>
            {(p) => (
              <A class="btn small" href={`/p/${encodeURIComponent(p())}/inbox?id=${m().id}`} title={`Open in the inbox of ${p()}`}>
                Inbox
              </A>
            )}
          </Show>
          <Show when={m().to_kind === "operator" && isUnread(m())}>
            <button class="small" onClick={() => props.onMarkRead()}>
              Mark read
            </button>
          </Show>
        </div>
      </div>
      <Show when={props.open}>
        <ThreadPanel thread={m().thread_id} row={m()} feed={props.feed} entryOf={props.entryOf} onCompose={props.onCompose} onError={props.onError} />
      </Show>
    </div>
  );
}

function ThreadPanel(props: { thread: string; row: Message; feed: MailFeed; entryOf: (id: string) => LogEntry | undefined; onCompose: (to?: string) => void; onError: (e: unknown) => void }) {
  const messages = createMemo(() => threadOf(props.feed.messages(), props.thread));
  const [text, setText] = createSignal("");
  const [sending, setSending] = createSignal(false);
  const to = () => counterpart(props.row);
  return (
    <div class="log-thread stack">
      <For each={messages()}>
        {(m) => (
          <div class={m.id === props.row.id ? "current" : undefined}>
            <MessageView message={m} held={props.entryOf(m.id)?.held} notice={props.entryOf(m.id)?.notice} />
          </div>
        )}
      </For>
      <form
        class="stack"
        style={{ gap: "6px" }}
        onSubmit={async (e) => {
          e.preventDefault();
          const last = messages().at(-1);
          if (!last || !text().trim()) return;
          setSending(true);
          try {
            // One message for each participant (docs/api.md §7.2). A reply from the operator ends a pause.
            const sent = await api.mail.reply(last.id, text());
            props.feed.upsert(sent);
            setText("");
            props.onError(null);
            await props.feed.pull([props.thread]);
            void attention.refresh();
          } catch (err) {
            props.onError(err);
          } finally {
            setSending(false);
          }
        }}
      >
        <textarea placeholder="Reply on the thread, to every participant" value={text()} onInput={(e) => setText(e.currentTarget.value)} aria-label="Reply" />
        <div class="row">
          <button class="primary" type="submit" disabled={!text().trim() || sending()}>
            Reply
          </button>
          <Show when={to()}>
            {(name) => (
              <button type="button" onClick={() => props.onCompose(name())}>
                New message to {name()}
              </button>
            )}
          </Show>
          <span class="muted small mono">thread {props.thread}</span>
        </div>
      </form>
    </div>
  );
}
