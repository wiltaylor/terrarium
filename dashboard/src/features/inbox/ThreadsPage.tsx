import { useInstance } from "../../instance";
// The Threads screen (TER-466, docs/api.md §6.13): the Inbox threads as a
// forum table (`GET /api/threads`). Each row has a status icon, the topic with
// the scope, the agent and the report-to under it, the messages, the open
// questions, the n/m chip of the linked cards (TER-147), and the last
// activity. A pending close proposal keeps Confirm and Decline on the row
// (TER-95). The threads that wait on you come first, then the newest
// activity. Filters: Open (the tracking threads too) and Archived (closed),
// and the project. `thread` and `inbox` events read the list again; a `card`
// event of a linked project reads the boards again. No polling.

import { useSearchParams } from "@solidjs/router";
import { A, useNavigate } from "../../instance/navigation";
import { createEffect, createSignal, For, Match, on, onCleanup, onMount, Show, Switch } from "solid-js";
import type { Board, Thread } from "../../api/types";
import { Empty, ErrorBanner, PageHead, Tabs, useNow } from "../../components/ui";
import { ago } from "../../lib/format";

import { CloseProposal, isConflict } from "./CloseProposal";
import { NewThreadDialog } from "./NewThreadDialog";
import { loadBoards, ProgressChip } from "./TrackedCards";
import { inThreadsView, lastActivity, openQuestionCount, pendingProposal, scopeLabel, sortThreads, threadHref, threadIcon, threadListStatus, threadsViewOf, waitsOnYou, type ThreadIconId, type ThreadsViewId } from "./threads";

const VIEW_TABS: readonly { id: ThreadsViewId; label: string }[] = [
  { id: "open", label: "Open" },
  { id: "archived", label: "Archived" },
];

const EMPTY: Record<ThreadsViewId, string> = {
  open: "No open threads. Open one with New thread.",
  archived: "No closed threads.",
};

export default function ThreadsPage() {
  const { api, events, inbox, projects } = useInstance();
  const now = useNow(30_000);
  const [params, setParams] = useSearchParams<{ status?: string; project?: string }>();
  const view = (): ThreadsViewId => threadsViewOf(params.status);
  const project = () => params.project ?? "";
  const navigate = useNavigate();
  const [newThread, setNewThread] = createSignal(false);
  const [list, setList] = createSignal<Thread[]>([]);
  const [loading, setLoading] = createSignal(true);
  const [error, setError] = createSignal<unknown>(null);
  const [boards, setBoards] = createSignal<Record<string, Board | undefined>>({});
  const projectsOf = (rows: Thread[]) => [...new Set(rows.flatMap((t) => (t.cards ?? []).map((c) => c.project)))];
  let boardGen = 0;
  const reloadBoards = async (rows: Thread[]) => {
    const mine = ++boardGen;
    const ps = projectsOf(rows);
    const b = ps.length ? await loadBoards(ps, rows.flatMap((t) => t.cards ?? []), api) : {};
    if (mine === boardGen) setBoards(b);
  };
  let gen = 0;
  const reload = async () => {
    const mine = ++gen;
    setLoading(true);
    try {
      const v = view();
      const rows = (await api.threads.list({ status: threadListStatus(v), ...(project() ? { scope: project() } : {}) })).filter((t) => inThreadsView(v, t));
      if (mine !== gen) return;
      setList(sortThreads(rows));
      setError(null);
      void reloadBoards(rows);
    } catch (e) {
      if (mine === gen) setError(e);
    } finally {
      if (mine === gen) setLoading(false);
    }
  };
  createEffect(on([view, project], () => void reload()));
  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    let tb: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (e.kind === "card" && e.project && projectsOf(list()).includes(e.project)) {
        clearTimeout(tb);
        tb = setTimeout(() => void reloadBoards(list()), 300);
        return;
      }
      if (e.kind !== "thread" && e.kind !== "inbox") return;
      clearTimeout(t);
      t = setTimeout(() => void reload(), 300);
    });
    onCleanup(() => (off(), clearTimeout(t), clearTimeout(tb)));
  });
  const changed = () => (void reload(), void inbox.refresh());

  return (
    <div class="stack">
      <PageHead title="Threads">
        <select aria-label="Project" value={project()} onChange={(e) => setParams({ project: e.currentTarget.value || undefined })}>
          <option value="">All projects</option>
          <For each={projects.projects()}>
            {(p) => (
              <option value={p.name} selected={p.name === project()}>
                {p.name}
              </option>
            )}
          </For>
        </select>
        <button onClick={() => void reload()}>Refresh</button>
        <button class="primary" onClick={() => setNewThread(true)} title="A conversation with one agent on one topic, with its questions inline">
          New thread
        </button>
      </PageHead>
      <p class="muted small" style={{ margin: 0 }}>
        A thread is a conversation with one agent on one topic. The topic is the opener's own words.
      </p>
      <Tabs tabs={VIEW_TABS} active={view()} onChange={(v) => setParams({ status: v === "open" ? undefined : v })} />
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={list().length} fallback={<Empty>{loading() ? "Loading…" : EMPTY[view()]}</Empty>}>
        <table class="threads-table" aria-label="Threads">
          <thead>
            <tr>
              <th class="threads-icon">
                <span class="sr-only">Status</span>
              </th>
              <th>Topic</th>
              <th class="num">Messages</th>
              <th class="num">Open questions</th>
              <th>Cards</th>
              <th>Last activity</th>
            </tr>
          </thead>
          <For each={list()}>{(t) => <ThreadTableRow thread={t} now={now()} boards={boards()} onChange={changed} />}</For>
        </table>
      </Show>
      <Show when={newThread()}>
        <NewThreadDialog
          scope={project() || undefined}
          onClose={() => setNewThread(false)}
          onOpened={(t) => {
            setNewThread(false);
            navigate(threadHref(t.id));
          }}
        />
      </Show>
    </div>
  );
}

/** One thread: its row, and a second row with the close proposal while one waits. */
function ThreadTableRow(props: { thread: Thread; now: number; boards: Record<string, Board | undefined>; onChange: () => void }) {
  const t = () => props.thread;
  const icon = () => threadIcon(t());
  const open = () => openQuestionCount(t());
  return (
    <tbody class={`thread-tr${waitsOnYou(t()) ? " waits" : ""}`} aria-label={`Thread ${t().id}`}>
      <tr>
        <td class="threads-icon">
          <span class={`thread-icon ${icon().icon}`} data-testid="thread-status" data-icon={icon().icon} title={icon().label} role="img" aria-label={icon().label}>
            <ThreadIcon icon={icon().icon} />
          </span>
        </td>
        <td class="thread-topic">
          <A href={threadHref(t().id)} style={{ "font-weight": 600 }}>
            {t().topic}
          </A>
          <div class="small muted thread-meta">
            <span>{scopeLabel(t().scope)}</span>
            <Show when={t().agent_name}>{(n) => <span>with {n()}</span>}</Show>
            <Show when={t().report_to_name}>{(n) => <span>reports to {n()}</span>}</Show>
          </div>
        </td>
        <td class="num" data-label="Messages">
          {t().message_count ?? "–"}
        </td>
        <td class="num" data-label="Open questions">
          <Show when={open() > 0} fallback={<span class="muted">0</span>}>
            <span class="warn-text">{open()}</span>
          </Show>
        </td>
        <td data-label="Cards">
          <ProgressChip thread={t()} boards={props.boards} />
        </td>
        <td data-label="Last activity">
          <span class="muted" title={lastActivity(t())}>
            {ago(lastActivity(t()), props.now)}
          </span>
        </td>
      </tr>
      <Show when={pendingProposal(t())}>
        <tr class="thread-proposal">
          <td />
          <td colSpan={5}>
            <CloseProposal thread={t()} now={props.now} onChanged={() => props.onChange()} onError={(e) => isConflict(e) && props.onChange()} />
          </td>
        </tr>
      </Show>
    </tbody>
  );
}

/** The status icons: inline SVG, no icon library (TER-466). */
function ThreadIcon(props: { icon: ThreadIconId }) {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <Switch>
        {/* A person: the thread waits on you. */}
        <Match when={props.icon === "you"}>
          <circle cx="12" cy="7" r="4" />
          <path d="M4 21v-1a6 6 0 0 1 6-6h4a6 6 0 0 1 6 6v1" />
        </Match>
        {/* A robot: the thread waits on the agent. */}
        <Match when={props.icon === "agent"}>
          <rect x="4" y="8" width="16" height="12" rx="2" />
          <path d="M12 4v4" />
          <circle cx="12" cy="3" r="1" />
          <circle cx="9" cy="13" r="1" />
          <circle cx="15" cy="13" r="1" />
          <path d="M9 17h6" />
        </Match>
        {/* Tracking: the work is handed off, the agent follows it. */}
        <Match when={props.icon === "tracking"}>
          <circle cx="12" cy="12" r="9" />
          <circle cx="12" cy="12" r="5" />
          <circle cx="12" cy="12" r="1" />
        </Match>
        {/* An archive box: closed. */}
        <Match when={props.icon === "closed"}>
          <rect x="3" y="4" width="18" height="4" rx="1" />
          <path d="M5 8v11a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1V8" />
          <path d="M10 12h4" />
        </Match>
      </Switch>
    </svg>
  );
}
