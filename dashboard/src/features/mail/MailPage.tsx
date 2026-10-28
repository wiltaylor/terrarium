// The mail log (docs/api.md §7): every message, newest first, with filters
// (from, to, project, thread, time range, free text) and a live tail. The
// log is a record: it keeps no unread state for the operator. Mail that
// needs the operator goes to the Inbox. A row opens its thread inline, with
// reply and compose.
//
// `/mail` shows every project. `/p/:project/mail` shows the messages that
// touch one project, and replies from the project inbox where the thread
// goes to it.

import { A, useParams, useSearchParams } from "@solidjs/router";
import { createEffect, createMemo, createSignal, For, on, onCleanup, Show } from "solid-js";
import { Empty, ErrorBanner, LoadMore, PageHead, useNow } from "../../components/ui";
import type { Card, Message } from "../../api/types";
import { ago } from "../../lib/format";
import { api, events, projects } from "../../stores";
import { createMailFeed, type MailFeed } from "./feed";
import {
  applyFilter,
  counterpart,
  filterFromParams,
  filterToParams,
  firstLine,
  namesOf,
  RANGE_LABELS,
  serverQuery,
  setTail,
  shortThread,
  TAIL_START,
  tailEvent,
  threadOf,
  type LogFilter,
  type LogParams,
  type TimeRange,
} from "./log";
import { Compose, MessageView, StateChips } from "./parts";

const PAGE = 50;

export default function MailPage() {
  const route = useParams<{ project?: string }>();
  const [params, setParams] = useSearchParams<LogParams>();
  /** The project of the route wins over the filter. */
  const filter = createMemo<LogFilter>(() => {
    const f = filterFromParams({ ...params });
    return route.project ? { ...f, project: route.project } : f;
  });
  const [limit, setLimit] = createSignal(PAGE);
  const setFilter = (patch: Partial<LogFilter>) => {
    const next = filterToParams({ ...filter(), ...patch });
    setParams({ ...next, ...(route.project ? { project: undefined } : {}) }, { replace: true });
    setLimit(PAGE);
  };
  const now = useNow(60_000);
  const feed = createMailFeed(api, () => serverQuery(filter()));
  const shown = createMemo(() => applyFilter(feed.messages(), filter(), now(), projects.prefixes()));
  const visible = () => shown().slice(0, limit());
  const names = createMemo(() => namesOf(feed.messages()));
  const [error, setError] = createSignal<unknown>(null);
  const [composing, setComposing] = createSignal<{ to?: string } | null>(null);
  const expanded = () => params.open;
  /** One open row: the newest shown row of the open thread. */
  const openRow = createMemo(() => (expanded() ? visible().find((m) => m.thread_id === expanded())?.id : undefined));
  const toggle = (thread: string) => setParams({ open: expanded() === thread ? undefined : thread });

  // Live tail: a `mail` event pulls the new messages and its thread again.
  // With the tail off, the log stays still and counts the events.
  const [tail, setTailState] = createSignal(TAIL_START);
  let t: ReturnType<typeof setTimeout> | undefined;
  const touched = new Set<string>();
  const pullSoon = (threads: string[]) => {
    threads.forEach((x) => touched.add(x));
    clearTimeout(t);
    t = setTimeout(() => {
      const list = [...touched];
      touched.clear();
      void feed.pull(list);
    }, 300);
  };
  const off = events.on((e) => {
    if (e.kind !== "mail") return;
    const thread = typeof e.data?.thread_id === "string" ? e.data.thread_id : undefined;
    const r = tailEvent(tail(), thread);
    setTailState(r.state);
    if (r.pull) pullSoon(thread ? [thread] : []);
  });
  onCleanup(() => (off(), clearTimeout(t)));
  const live = (on: boolean) => {
    const r = setTail(tail(), on);
    setTailState(r.state);
    if (r.pull) pullSoon(r.pull);
  };

  // Opening a thread loads all of it: a filter can hide part of it.
  createEffect(
    on(expanded, (thread) => {
      if (thread) void feed.pull([thread]);
    }),
  );

  return (
    <>
      <PageHead title={route.project ? `Mail · ${route.project}` : "Mail log"}>
        <label class="row small" title="Show new messages as they come">
          <input type="checkbox" checked={tail().live} onChange={(e) => live(e.currentTarget.checked)} /> Live tail
        </label>
        <Show when={!tail().live && tail().missed > 0}>
          <button class="small" onClick={() => live(true)}>
            {tail().missed} new
          </button>
        </Show>
        <button onClick={() => void feed.reload()}>Refresh</button>
        <button class="primary" onClick={() => setComposing(route.project ? { to: `project:${route.project}` } : {})}>
          Compose
        </button>
      </PageHead>

      <div class="row log-filters" role="search">
        <input list="mail-log-names" placeholder="From" aria-label="From" value={filter().from} onInput={(e) => setFilter({ from: e.currentTarget.value })} />
        <input list="mail-log-names" placeholder="To" aria-label="To" value={filter().to} onInput={(e) => setFilter({ to: e.currentTarget.value })} />
        <datalist id="mail-log-names">
          <For each={names()}>{(n) => <option value={n} />}</For>
        </datalist>
        <Show when={!route.project}>
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
        </Show>
        <input placeholder="Thread ID" aria-label="Thread" class="mono" value={filter().thread} onInput={(e) => setFilter({ thread: e.currentTarget.value.trim() })} />
        <select aria-label="Time range" value={filter().range} onChange={(e) => setFilter({ range: e.currentTarget.value as TimeRange })}>
          <For each={Object.entries(RANGE_LABELS)}>
            {([k, label]) => (
              <option value={k} selected={k === filter().range}>
                {label}
              </option>
            )}
          </For>
        </select>
        <Show when={filter().range === "custom"}>
          <input type="datetime-local" aria-label="From time" value={filter().since} onInput={(e) => setFilter({ since: e.currentTarget.value })} />
          <input type="datetime-local" aria-label="To time" value={filter().until} onInput={(e) => setFilter({ until: e.currentTarget.value })} />
        </Show>
        <input type="search" placeholder="Search subject, body, names" aria-label="Search" value={filter().q} onInput={(e) => setFilter({ q: e.currentTarget.value })} />
      </div>

      <ErrorBanner error={error() ?? feed.error()} onDismiss={() => setError(null)} />

      <div class="card-box table-wrap" style={{ padding: "0" }}>
        <table class="mail-log">
          <thead>
            <tr>
              <th>Time</th>
              <th>From</th>
              <th>To</th>
              <th>Subject</th>
              <th>Thread</th>
            </tr>
          </thead>
          <tbody>
            <For
              each={visible()}
              fallback={
                <tr>
                  <td colspan={5}>
                    <Empty>{!feed.loaded() ? "Loading…" : "No mail matches."}</Empty>
                  </td>
                </tr>
              }
            >
              {(m) => (
                <LogRow
                  message={m}
                  now={now()}
                  open={openRow() === m.id}
                  project={route.project}
                  onToggle={() => toggle(m.thread_id)}
                  onThread={() => setFilter({ thread: m.thread_id })}
                  feed={feed}
                  onCompose={(to) => setComposing({ to })}
                  onError={setError}
                />
              )}
            </For>
          </tbody>
        </table>
      </div>
      <LoadMore hasMore={shown().length > limit()} onMore={() => setLimit((n) => n + PAGE)} count={visible().length} />

      <Show when={composing()}>{(c) => <Compose to={c().to} onClose={() => setComposing(null)} onSent={(m) => feed.upsert([m])} />}</Show>
    </>
  );
}

function LogRow(props: {
  message: Message;
  now: number;
  open: boolean;
  project?: string;
  onToggle: () => void;
  onThread: () => void;
  feed: MailFeed;
  onCompose: (to?: string) => void;
  onError: (e: unknown) => void;
}) {
  const m = () => props.message;
  return (
    <>
      <tr class={`log-row${props.open ? " open" : ""}`} onClick={() => props.onToggle()}>
        <td class="muted small nowrap" title={m().created_at}>
          {ago(m().created_at, props.now)}
        </td>
        <td class="small" title={m().from}>
          {m().from_name}
        </td>
        <td class="small" title={m().to}>
          {m().to_name}
        </td>
        <td class="log-subject-cell">
          <button class="log-main" onClick={(e) => (e.stopPropagation(), props.onToggle())} aria-expanded={props.open}>
            <span class="log-subject">
              <span class="subject">{m().subject || "(no subject)"}</span>
              <span class="muted"> {firstLine(m().body)}</span>
            </span>
          </button>
          <StateChips message={m()} />
        </td>
        <td>
          <button class="small ghost mono" title={`Show only thread ${m().thread_id}`} onClick={(e) => (e.stopPropagation(), props.onThread())}>
            {shortThread(m().thread_id)}
          </button>
        </td>
      </tr>
      <Show when={props.open}>
        <tr class="log-thread-row">
          <td colspan={5}>
            <ThreadPanel thread={m().thread_id} row={m()} project={props.project} feed={props.feed} onCompose={props.onCompose} onError={props.onError} />
          </td>
        </tr>
      </Show>
    </>
  );
}

function ThreadPanel(props: { thread: string; row: Message; project?: string; feed: MailFeed; onCompose: (to?: string) => void; onError: (e: unknown) => void }) {
  const messages = createMemo(() => threadOf(props.feed.messages(), props.thread));
  const [text, setText] = createSignal("");
  const [sending, setSending] = createSignal(false);
  const [made, setMade] = createSignal<Card | null>(null);
  const to = () => counterpart(props.row);
  /** The newest message to the inbox of the page's project: the reply goes from that inbox, and a card can come from it. */
  const toInbox = () => (props.project ? messages().filter((m) => m.to === `project:${props.project}`).at(-1) : undefined);
  const makeCard = async (m: Message) => {
    try {
      // The daemon takes the title and the body from the message, and links its thread.
      setMade(await api.cards.create(props.project!, { from_mail: m.id }));
    } catch (e) {
      props.onError(e);
    }
  };
  return (
    <div class="log-thread stack">
      <For each={messages()}>
        {(m) => (
          <div class={m.id === props.row.id ? "current" : undefined}>
            <MessageView message={m} />
          </div>
        )}
      </For>
      <Show when={toInbox()}>
        {(m) => (
          <div class="row">
            <Show
              when={made()}
              fallback={
                <button onClick={() => void makeCard(m())} title="Make a card from the newest message to the project inbox">
                  Make card
                </button>
              }
            >
              {(c) => (
                <span class="notice">
                  Created <A href={`/p/${encodeURIComponent(props.project!)}/board?card=${c().id}`}>{c().id}</A>. It links this thread.
                </span>
              )}
            </Show>
          </div>
        )}
      </Show>
      <form
        class="stack"
        style={{ gap: "6px" }}
        onSubmit={async (e) => {
          e.preventDefault();
          const last = messages().at(-1);
          if (!last || !text().trim()) return;
          setSending(true);
          try {
            // One message for each participant (docs/api.md §7.2). From the project inbox when the thread goes to it.
            const inbox = toInbox();
            const sent = inbox ? await api.projectInbox.reply(props.project!, inbox.id, text()) : await api.mail.reply(last.id, text());
            props.feed.upsert(sent);
            setText("");
            props.onError(null);
            await props.feed.pull([props.thread]);
          } catch (err) {
            props.onError(err);
          } finally {
            setSending(false);
          }
        }}
      >
        <textarea
          placeholder={toInbox() ? `Reply on the thread as project:${props.project}` : "Reply on the thread, to every participant"}
          value={text()}
          onInput={(e) => setText(e.currentTarget.value)}
          aria-label="Reply"
        />
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
