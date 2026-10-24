import { A, useParams, useSearchParams } from "@solidjs/router";
import { createSignal, For, onCleanup, Show } from "solid-js";
import { createPagedList } from "../../components/paged";
import { Empty, ErrorBanner, LoadMore, PageHead } from "../../components/ui";
import type { Card, Message } from "../../api/types";
import { ago } from "../../lib/format";
import { api, events, unread } from "../../stores";
import { MessageView } from "../mail/MailPage";

const isUnread = (m: Message) => m.state === "delivered" || m.state === "queued";

export default function InboxPage() {
  const params = useParams<{ project: string }>();
  const [search, setSearch] = useSearchParams<{ id?: string }>();
  const [onlyUnread, setOnlyUnread] = createSignal(false);
  const inbox = createPagedList(
    () => `${params.project}|${onlyUnread()}`,
    (_k, cursor) => api.inbox.list(params.project, { unread: onlyUnread() || undefined, cursor }),
    (m) => m.id,
  );
  const [error, setError] = createSignal<unknown>(null);
  const [made, setMade] = createSignal<Record<string, Card>>({});
  const [reply, setReply] = createSignal("");
  // Newest first for the list; the API answers oldest first.
  const shown = () => inbox.items().slice().reverse();
  const selected = () => inbox.items().find((m) => m.id === search.id) ?? shown()[0];

  let t: ReturnType<typeof setTimeout> | undefined;
  const off = events.on((e) => {
    if (e.kind === "mail" && (e.project === params.project || e.data?.to === `project:${params.project}`)) {
      clearTimeout(t);
      t = setTimeout(() => void inbox.reload(), 300);
    }
  });
  onCleanup(() => (off(), clearTimeout(t)));

  const markRead = async (m: Message) => {
    if (!isUnread(m)) return;
    try {
      inbox.upsert([await api.inbox.markRead(params.project, m.id)]);
      void unread.refresh();
    } catch (e) {
      setError(e);
    }
  };

  const makeCard = async (m: Message) => {
    try {
      // The daemon takes the title and the body from the message, and links its thread.
      const card = await api.cards.create(params.project, { from_mail: m.id });
      setMade({ ...made(), [m.id]: card });
      await markRead(m);
    } catch (e) {
      setError(e);
    }
  };

  return (
    <>
      <PageHead title={`Inbox · ${params.project}`}>
        <span class="muted small">project:{params.project}</span>
        <label class="row small">
          <input type="checkbox" checked={onlyUnread()} onChange={(e) => setOnlyUnread(e.currentTarget.checked)} /> Unread only
        </label>
        <button onClick={() => inbox.reload()}>Refresh</button>
      </PageHead>
      <ErrorBanner error={error() ?? inbox.error()} onDismiss={() => setError(null)} />
      <div class="split">
        <div class="card-box thread-list" style={{ padding: "4px" }}>
          <For each={shown()} fallback={<Empty>{inbox.initial() ? "Loading…" : "The inbox is empty."}</Empty>}>
            {(m) => (
              <button class={`${selected()?.id === m.id ? "active" : ""} ${isUnread(m) ? "unread" : ""}`} onClick={() => (setSearch({ id: m.id }), void markRead(m))}>
                <span class="subject">{m.subject || "(no subject)"}</span>
                <span class="muted small">
                  from {m.from_name} · {ago(m.created_at)}
                </span>
              </button>
            )}
          </For>
          <LoadMore hasMore={inbox.hasMore()} loading={inbox.loading()} onMore={inbox.loadMore} count={inbox.items().length} />
        </div>
        <Show when={selected()}>
          {(m) => (
            <div class="stack">
              <MessageView message={m()} />
              <div class="row">
                <Show
                  when={made()[m().id]}
                  fallback={
                    <button class="primary" onClick={() => makeCard(m())}>
                      Make card
                    </button>
                  }
                >
                  {(c) => (
                    <span class="notice">
                      Created <A href={`/p/${params.project}/board?card=${c().id}`}>{c().id}</A>. It links thread <span class="mono small">{m().thread_id}</span>.
                    </span>
                  )}
                </Show>
              </div>
              <form
                class="stack"
                style={{ gap: "6px" }}
                onSubmit={async (e) => {
                  e.preventDefault();
                  if (!reply().trim()) return;
                  try {
                    // From project:<name>, one message for each participant.
                    await api.inbox.reply(params.project, m().id, reply());
                    setReply("");
                    void inbox.reload();
                  } catch (err) {
                    setError(err);
                  }
                }}
              >
                <textarea placeholder={`Reply on the thread as project:${params.project}`} value={reply()} onInput={(e) => setReply(e.currentTarget.value)} aria-label="Reply" />
                <div>
                  <button type="submit" disabled={!reply().trim()}>
                    Reply
                  </button>
                </div>
              </form>
            </div>
          )}
        </Show>
      </div>
    </>
  );
}
