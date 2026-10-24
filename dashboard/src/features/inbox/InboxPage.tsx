import { A, useParams, useSearchParams } from "@solidjs/router";
import { createResource, createSignal, For, Show } from "solid-js";
import { Empty, ErrorBanner, PageHead } from "../../components/ui";
import type { Card } from "../../api/types";
import { ago } from "../../lib/format";
import { api } from "../../stores";
import { MessageView } from "../mail/MailPage";

export default function InboxPage() {
  const params = useParams<{ project: string }>();
  const [search, setSearch] = useSearchParams<{ id?: string }>();
  const [inbox, { refetch }] = createResource(() => params.project, (p) => api.inbox.list(p));
  const [error, setError] = createSignal<string | null>(null);
  const [made, setMade] = createSignal<Record<string, Card>>({});
  const [reply, setReply] = createSignal("");
  const selected = () => inbox()?.find((m) => m.id === search.id) ?? inbox()?.[0];

  const makeCard = async (id: string) => {
    const m = inbox()?.find((x) => x.id === id);
    if (!m) return;
    try {
      const card = await api.cards.create(params.project, { title: m.subject, body: `From ${m.from}:\n\n${m.body}`, from_mail: m.id, column: "backlog" });
      setMade({ ...made(), [id]: card });
      await api.mail.markRead(id);
      void refetch();
    } catch (e) {
      setError((e as Error).message);
    }
  };

  return (
    <>
      <PageHead title={`Inbox · ${params.project}`}>
        <span class="muted small">project:{params.project}</span>
        <button onClick={refetch}>Refresh</button>
      </PageHead>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <div class="split">
        <div class="card-box thread-list" style={{ padding: "4px" }}>
          <For each={inbox()} fallback={<Empty>The inbox is empty.</Empty>}>
            {(m) => (
              <button class={`${selected()?.id === m.id ? "active" : ""} ${m.state === "delivered" || m.state === "queued" ? "unread" : ""}`} onClick={() => setSearch({ id: m.id })}>
                <span class="subject">{m.subject}</span>
                <span class="muted small">
                  from {m.from} · {ago(m.created_at)}
                </span>
              </button>
            )}
          </For>
        </div>
        <Show when={selected()}>
          {(m) => (
            <div class="stack">
              <MessageView message={m()} />
              <div class="row">
                <Show
                  when={made()[m().id]}
                  fallback={
                    <button class="primary" onClick={() => makeCard(m().id)}>
                      Make card
                    </button>
                  }
                >
                  {(c) => (
                    <span class="notice">
                      Created <A href={`/p/${params.project}/board?card=${c().id}`}>{c().id}</A>. It carries thread {m().thread_id}.
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
                  await api.mail.reply(m().id, reply());
                  setReply("");
                  void refetch();
                }}
              >
                <textarea placeholder="Reply on the thread" value={reply()} onInput={(e) => setReply(e.currentTarget.value)} aria-label="Reply" />
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
