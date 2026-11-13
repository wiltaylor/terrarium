import { useInstance } from "../../instance";
import { ReadBoundary, useReadBoundary } from "../../components/ReadBoundary";
import { PostEditor } from "../../components/PostEditor";
// The Mail tab of the Logs screen (docs/api.md §7): every message, newest
// first, with filters (from, to, project, thread, time range, free text)
// and the live mode of the log tabs (components/Live.tsx). The log is a record: it keeps no unread state for the
// operator. Mail that needs the operator goes to the Inbox. A row opens its
// thread inline, with reply and compose.
//
// `/logs/mail` shows every project. `/p/:project/logs/mail` shows the
// messages that touch one project, and replies from the project inbox where
// the thread goes to it.

import { useParams, useSearchParams } from "@solidjs/router";
import { A } from "../../instance/navigation";
import { createEffect, createMemo, createSignal, createUniqueId, For, on, onCleanup, Show } from "solid-js";
import { createLive, createScrollKeeper, LiveControl, NewAbove } from "../../components/Live";
import { SessionLink } from "../../components/SessionLink";
import { Empty, ErrorBanner, LoadMore, PageHead, useNow } from "../../components/ui";
import type { Card, Message } from "../../api/types";
import { ago } from "../../lib/format";

import { createMailFeed, type MailFeed } from "./feed";
import {
  applyFilter,
  counterpart,
  filterFromParams,
  filterToParams,
  firstLine,
  namesOf,
  openRowOf,
  openThreadOf,
  RANGE_LABELS,
  serverQuery,
  shortThread,
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
  return <MailContent project={route.project} params={params} onParams={setParams} />;
}

/** `search` seeds the local free-text filter. Only a changed configured value replaces it. */
/**
 * `canMakeCard` (write_project) gates Make card, `canReplyAsProject`
 * (project_admin, the inbox reply) gates a reply that goes out as the project
 * inbox (TER-1134). Compose and other replies follow the mail scope of the
 * server, not a project right, and stay.
 */
export function MailPanel(props: { project: string; search?: string } & MailWrites) {
  return <Show when={props.project} keyed>{(project) => <LocalMail project={project} search={props.search} canMakeCard={props.canMakeCard} canReplyAsProject={props.canReplyAsProject} />}</Show>;
}

/** The route leaves them out and shows every control. */
type MailWrites = { canMakeCard?: boolean; canReplyAsProject?: boolean };

function LocalMail(props: { project: string; search?: string } & MailWrites) {
  const [params, setParams] = createSignal<LogParams>(props.search ? { q: props.search } : {});
  createEffect(on(() => props.search, (search, old) => { if (search !== old) setParams((p) => ({ ...p, q: search || undefined })); }, { defer: true }));
  /** Message posts of this panel get ids of their own (see `mailPostId`). */
  const idScope = `panel-${createUniqueId()}`;
  return <MailContent project={props.project} idScope={idScope} serverProject params={params()} onParams={(patch) => setParams((old) => ({ ...old, ...patch }))} canMakeCard={props.canMakeCard} canReplyAsProject={props.canReplyAsProject} />;
}

/**
 * `serverProject`: the daemon filters the messages to the project too (the
 * page widget, TER-1077), so no other project's mail reaches the browser. The
 * route keeps its browser filter only.
 */
function MailContent(props: { project?: string; idScope?: string; serverProject?: boolean; params: LogParams; onParams: (patch: LogParams, options?: { replace: boolean }) => void } & MailWrites) {
  const { api, projects, events } = useInstance();
  const namesId = `mail-log-names-${createUniqueId()}`;
  /** The project of the route wins over the filter. */
  const filter = createMemo<LogFilter>(() => {
    const f = filterFromParams({ ...props.params });
    return props.project ? { ...f, project: props.project } : f;
  });
  const [limit, setLimit] = createSignal(PAGE);
  const setFilter = (patch: Partial<LogFilter>) => {
    const next = filterToParams({ ...filter(), ...patch });
    props.onParams({ ...next, ...(props.project ? { project: undefined } : {}) }, { replace: true });
    setLimit(PAGE);
  };
  const now = useNow(60_000);
  const live = createLive();
  const feed = createMailFeed(api, () => ({ ...serverQuery(filter()), ...(props.serverProject && props.project ? { project: props.project } : {}) }), { onNew: live.arrive });
  /** Every message that the filters let through, held ones too. */
  const matching = createMemo(() => applyFilter(feed.messages(), filter(), now(), projects.prefixes()));
  const shown = createMemo(() => live.shown(matching(), (m) => m.id));
  /** "Show N new": the held messages that match the filters; the others stay held. */
  const showHeld = () => live.release(matching().map((m) => m.id));
  const visible = () => shown().slice(0, limit());
  const names = createMemo(() => namesOf(feed.messages()));
  const [error, setError] = createSignal<unknown>(null);
  const [composing, setComposing] = createSignal<{ to?: string } | null>(null);
  /** `?open=`: the clicked message, or a thread from a link. */
  const expanded = () => props.params.open;
  /** One open row: the clicked row, or the newest shown row of a linked thread. */
  const openRow = createMemo(() => openRowOf(visible(), expanded())?.id);
  const openThread = createMemo(() => openThreadOf(feed.messages(), expanded()));
  const toggle = (m: Message) => props.onParams({ open: openRow() === m.id ? undefined : m.id });

  // A `mail` event pulls the new messages and its thread again, live or
  // not: off, the new messages wait (held) and "Show N new" counts the ones
  // that match the filters.
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
    pullSoon(thread ? [thread] : []);
  });
  onCleanup(() => (off(), clearTimeout(t)));
  const keeper = createScrollKeeper("tr.log-row");

  // Opening a thread loads all of it: a filter can hide part of it.
  createEffect(
    on(openThread, (thread) => {
      if (thread) void feed.pull([thread]);
    }),
  );

  return (
    <>
      <PageHead title="Mail" level={2}>
        <LiveControl live={live} count={live.held(matching(), (m) => m.id)} onShow={showHeld} />
        <button onClick={() => (live.release(), void feed.reload())}>Refresh</button>
        <button class="primary" onClick={() => setComposing(props.project ? { to: `project:${props.project}` } : {})}>
          Compose
        </button>
      </PageHead>

      <div class="row log-filters" role="search">
        <input list={namesId} placeholder="From" aria-label="From" value={filter().from} onInput={(e) => setFilter({ from: e.currentTarget.value })} />
        <input list={namesId} placeholder="To" aria-label="To" value={filter().to} onInput={(e) => setFilter({ to: e.currentTarget.value })} />
        <datalist id={namesId}>
          <For each={names()}>{(n) => <option value={n} />}</For>
        </datalist>
        <Show when={!props.project}>
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

      <NewAbove live={live} keeper={keeper} held={live.held(matching(), (m) => m.id)} onShow={showHeld} />
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
          <tbody ref={keeper.ref}>
            <For
              each={visible().map((m) => m.id)}
              fallback={
                <tr>
                  <td colspan={5}>
                    <Empty>{!feed.loaded() ? "Loading…" : "No mail matches."}</Empty>
                  </td>
                </tr>
              }
            >
              {(id) => {
                // Refreshes replace message objects, not the row or its reply draft.
                const m = () => feed.messages().find((message) => message.id === id)!;
                return (
                <LogRow
                  message={m()}
                  now={now()}
                  open={openRow() === id}
                  project={props.project}
                  idScope={props.idScope}
                  onToggle={() => toggle(m())}
                  onThread={() => setFilter({ thread: m().thread_id })}
                  feed={feed}
                  onCompose={(to) => setComposing({ to })}
                  onError={setError}
                  canMakeCard={props.canMakeCard}
                  canReplyAsProject={props.canReplyAsProject}
                />
                );
              }}
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
  idScope?: string;
  onToggle: () => void;
  onThread: () => void;
  feed: MailFeed;
  onCompose: (to?: string) => void;
  onError: (e: unknown) => void;
} & MailWrites) {
  const m = () => props.message;
  return (
    <>
      <tr class={`log-row${props.open ? " open" : ""}`} onClick={() => props.onToggle()}>
        <td class="muted small nowrap" title={m().created_at}>
          {ago(m().created_at, props.now)}
        </td>
        <td class="small">
          <span class="log-who">
            <span title={m().from}>{m().from_name}</span>
            <SessionLink row={m()} />
          </span>
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
            <ThreadPanel thread={m().thread_id} row={m()} project={props.project} idScope={props.idScope} feed={props.feed} onCompose={props.onCompose} onError={props.onError} canMakeCard={props.canMakeCard} canReplyAsProject={props.canReplyAsProject} />
          </td>
        </tr>
      </Show>
    </>
  );
}

function ThreadPanel(props: { thread: string; row: Message; project?: string; idScope?: string; feed: MailFeed; onCompose: (to?: string) => void; onError: (e: unknown) => void } & MailWrites) {
  const { api } = useInstance();
  const messages = createMemo(() => threadOf(props.feed.messages(), props.thread));
  const read = useReadBoundary("mail", () => props.thread, () => messages().map((m) => m.created_at));
  const [text, setText] = createSignal("");
  let replyInput: HTMLTextAreaElement | undefined;
  const [sending, setSending] = createSignal(false);
  const [made, setMade] = createSignal<Card | null>(null);
  const to = () => counterpart(props.row);
  /** The newest message to the inbox of the page's project: the reply goes from that inbox, and a card can come from it. */
  const toInbox = () => (props.project ? messages().filter((m) => m.to === `project:${props.project}`).at(-1) : undefined);
  /** A reply from the project inbox needs its admin right; another reply follows the mail scope. */
  const canReply = () => !toInbox() || props.canReplyAsProject !== false;
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
      <ErrorBanner error={read.error()} />
      <For each={messages()}>
        {(m, index) => (
          <>
          <Show when={index() === read.first()}><ReadBoundary /></Show>
          <div class={m.id === props.row.id ? "current" : undefined}>
            <MessageView message={m} number={index() + 1} project={props.project} idScope={props.idScope} onQuote={(quote) => {
              setText((old) => old + (old ? "\n\n" : "") + quote);
              replyInput?.focus();
            }} />
          </div>
          </>
        )}
      </For>
      <Show when={props.canMakeCard !== false ? toInbox() : undefined}>
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
          if (!last || !text().trim() || !canReply()) return;
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
        <Show when={canReply()}>
        <PostEditor label="Reply" text={text()} project={props.project ?? ""} textBody>
        <textarea ref={replyInput}
          placeholder={toInbox() ? `Reply on the thread as project:${props.project}` : "Reply on the thread, to every participant"}
          value={text()}
          onInput={(e) => setText(e.currentTarget.value)}
          aria-label="Reply"
        />
        </PostEditor>
        </Show>
        <div class="row">
          <Show when={canReply()}>
            <button class="primary" type="submit" disabled={!text().trim() || sending()}>
              Reply
            </button>
          </Show>
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
