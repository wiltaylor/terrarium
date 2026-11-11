import { useInstance } from "../../instance";
// The Sessions tab of the agent page (docs/api.md §6.3): the sessions of the
// agent, one row for each harness session (its resumes counted on the row),
// and a read-only viewer of one transcript.
// The open session is in the URL (`?tab=sessions&session=<id>`).

import { createEffect, createMemo, For, on, Show } from "solid-js";
import type { Agent, Session } from "../../api/types";
import { createPagedList } from "../../components/paged";
import { ErrorBanner, LoadMore } from "../../components/ui";
import { ago } from "../../lib/format";

import { createTranscriptView, StreamChip, TranscriptLog } from "./conversation/TranscriptLog";
import { findGroup, groupChip, groupSessions, sessionLabel, type SessionGroup } from "./sessions";

export function SessionsPanel(props: {
  agent: Agent;
  /** The open session (a row ID or a harness session ID), or `null` for the list. */
  open: string | null;
  onOpen: (id: string | null) => void;
  onOpenConversation: () => void;
}) {
  const { api } = useInstance();
  const sessions = createPagedList(
    () => props.agent.id,
    (aid, cursor) => api.agents.sessions(aid, { cursor }),
    (s) => s.id,
  );
  // A new session shows up as a new harness session or a status change.
  createEffect(on(() => props.agent.session, () => void sessions.reload(), { defer: true }));
  const groups = createMemo(() => groupSessions(sessions.items()));
  const openGroup = createMemo(() => (props.open ? findGroup(groups(), props.open) : undefined));

  return (
    <Show
      when={props.open}
      fallback={
        <>
          <ErrorBanner error={sessions.error()} />
          <SessionTable groups={groups()} onOpen={(g) => props.onOpen(g.session.id)} onOpenConversation={props.onOpenConversation} />
          <LoadMore hasMore={sessions.hasMore()} loading={sessions.loading()} onMore={sessions.loadMore} />
          <Show when={!sessions.loading() && !sessions.initial() && groups().length === 0}>
            <p class="muted small">No sessions yet. A turn or mail wakes the agent.</p>
          </Show>
        </>
      }
    >
      {(id) => (
        <SessionViewer
          agent={props.agent}
          id={id()}
          group={openGroup()}
          listLoading={sessions.loading() || sessions.initial()}
          onBack={() => props.onOpen(null)}
          onOpenConversation={props.onOpenConversation}
        />
      )}
    </Show>
  );
}

function SessionTable(props: { groups: SessionGroup[]; onOpen: (g: SessionGroup) => void; onOpenConversation: () => void }) {
  return (
    <div class="table-wrap">
      <table class="sessions-table">
        <thead>
          <tr>
            <th>Started</th>
            <th>Turns</th>
            <th>Title</th>
            <th>State</th>
            <th>
              <span class="sr-only">Actions</span>
            </th>
          </tr>
        </thead>
        <tbody>
          <For each={props.groups}>
            {(g) => (
              <tr class="clickable" onClick={() => props.onOpen(g)}>
                <td title={g.started_at}>{ago(g.started_at)}</td>
                <td>{g.turns}</td>
                <td class="sessions-title">
                  {/* The row is clickable; the button is the keyboard way in. */}
                  <button
                    type="button"
                    class="link"
                    title={g.session.harness_session_id ?? g.session.id}
                    onClick={(e) => {
                      e.stopPropagation();
                      props.onOpen(g);
                    }}
                  >
                    {sessionLabel({ ...g.session, title: g.title })}
                  </button>
                  <Show when={g.resumes > 0}>
                    <span class="muted small" title="A restart or a daemon restart resumed this harness session.">
                      {" "}
                      · resumed {g.resumes}×
                    </span>
                  </Show>
                  <Show when={g.session.transcript === false}>
                    <span class="chip warn">no transcript</span>
                  </Show>
                </td>
                <td>
                  <Show when={groupChip(g)}>{(c) => <span class={`chip ${c().tone}`}>{c().label}</span>}</Show>
                </td>
                <td>
                  <Show when={g.active || g.current}>
                    <button
                      type="button"
                      class="small"
                      onClick={(e) => {
                        e.stopPropagation();
                        props.onOpenConversation();
                      }}
                    >
                      Open in Conversation
                    </button>
                  </Show>
                </td>
              </tr>
            )}
          </For>
        </tbody>
      </table>
    </div>
  );
}

/** One session's transcript, read-only: the same blocks as the Conversation tab, no composer. */
function SessionViewer(props: {
  agent: Agent;
  id: string;
  group: SessionGroup | undefined;
  listLoading: boolean;
  onBack: () => void;
  onOpenConversation: () => void;
}) {
  // The list's row when it has the session (with `ended_at`, so an ended session opens no stream). Else,
  // once the first page is in, a bare row, which the transcript endpoint also takes.
  const session = createMemo<Session | undefined>(
    () => props.group?.session ?? (props.listLoading ? undefined : { id: props.id, agent: props.agent.id, started_at: "", turns: 0, current: false }),
    undefined,
    { equals: (a, b) => a?.id === b?.id },
  );
  const view = createTranscriptView(() => props.agent, session);
  const g = () => props.group;

  return (
    <section class="convo-main" aria-label="Session transcript">
      <div class="convo-strip">
        <button type="button" class="link small" onClick={() => props.onBack()}>
          ← Back to sessions
        </button>
        <Show when={g()?.active || g()?.current}>
          <button type="button" class="small" onClick={() => props.onOpenConversation()}>
            Open in Conversation
          </button>
        </Show>
      </div>
      <div class="card-box session-head">
        <strong class="session-title">{g() ? sessionLabel({ ...g()!.session, title: g()!.title }) : props.id}</strong>
        <dl class="kv small">
          <dt>Session</dt>
          <dd class="mono">{g()?.session.id ?? props.id}</dd>
          <Show when={g()?.session.harness_session_id}>
            {(h) => (
              <>
                <dt>Harness session</dt>
                <dd class="mono">{h()}</dd>
              </>
            )}
          </Show>
          <Show when={g()}>
            {(gr) => (
              <>
                <dt>Started</dt>
                <dd title={gr().started_at}>
                  {new Date(gr().started_at).toLocaleString()} ({ago(gr().started_at)})
                </dd>
                <dt>Turns</dt>
                <dd>
                  {gr().turns}
                  <Show when={gr().resumes > 0}>
                    <span class="muted"> · resumed {gr().resumes}×</span>
                  </Show>
                </dd>
              </>
            )}
          </Show>
          <dt>State</dt>
          <dd class="row" style={{ gap: "6px" }}>
            <Show when={g() && groupChip(g()!)}>{(c) => <span class={`chip ${c().tone}`}>{c().label}</span>}</Show>
            <StreamChip state={view.t.streamState()} ended={!view.t.active()} />
          </dd>
        </dl>
        <Show when={!g() && !props.listLoading}>
          <p class="muted small" style={{ margin: 0 }}>
            This session is not in the first page of the list.
          </p>
        </Show>
      </div>
      <TranscriptLog view={view} label="Session transcript" empty={<p class="muted convo-note">Loading…</p>} />
    </section>
  );
}
