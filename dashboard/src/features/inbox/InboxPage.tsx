// The Inbox: everything that waits for the operator (`GET /api/inbox`).
// Questions, bundles of questions, and notices of the leaders, mail to `operator`, and threads that
// the loop guard paused. Tabs pick the kind and Open or Done; the project
// filter picks one project. `inbox` and `question` events read the list again.
// The Inbox threads (docs/api.md §6.13) have their own Threads screen (TER-466): an old
// `?tab=thread` link redirects there.

import { A, Navigate, useLocation, useSearchParams, useNavigate } from "@solidjs/router";
import { createEffect, createMemo, createSignal, For, Match, on, onCleanup, onMount, Show, Switch } from "solid-js";
import { createStore, reconcile } from "solid-js/store";
import type { InboxItem, InboxWatch, Message, Question } from "../../api/types";
import { Empty, ErrorBanner, PageHead, Tabs, useNow } from "../../components/ui";
import { api, events, inbox, projects } from "../../stores";
import { MailItemCard, PausedThreadCard, WatchCard } from "./cards";
import { AskDialog } from "./AskDialog";
import { MessageDialog } from "./MessageDialog";
import { canForward, emptyText, INBOX_TABS, inboxQuery, isInboxEvent, matchesTabs, putItem, questionFlags, sortItems, STATUS_TABS, statusTabOf, type InboxStatusTab, type InboxTab } from "./model";
import { inboxRedirect } from "../../app/nav";
import { questionItemId } from "./questions";
import { inboxItemDomId } from "./critical";
import { BundleCard } from "./BundleCard";
import { QuestionCard } from "./QuestionCard";
import { byInboxOrder } from "./model";

const TAB_IDS = INBOX_TABS.map((t) => t.id) as string[];
const PAGE_SIZE = 40;
const FETCH_SIZE = 200;
const cursorOf = (i: InboxItem) => JSON.stringify([i.open, !!i.critical, i.urgent, i.at, i.id]);

/** The Inbox, or the redirect of an old `?tab=thread` link to the Threads screen. */
export default function InboxPage() {
  const location = useLocation();
  return (
    <Show when={inboxRedirect(location.search)} fallback={<InboxScreen />}>
      {(to) => <Navigate href={to()} />}
    </Show>
  );
}

function InboxScreen() {
  const now = useNow(30_000);
  const [params, setParams] = useSearchParams<{ tab?: string; status?: string; project?: string; item?: string }>();
  // `?tab=watch` (the Asked kind tab of 0.1.0) opens the Asked status tab, or Done with the asks.
  const status = (): InboxStatusTab => statusTabOf(params.status, params.tab);
  const tab = (): InboxTab => (TAB_IDS.includes(params.tab ?? "") ? (params.tab as InboxTab) : params.tab === "watch" && status() === "done" ? "watch" : "all");
  const project = () => params.project ?? "";
  const keep = (i: InboxItem) => matchesTabs(i, tab(), status(), project());

  // The items in a store, reconciled by `id` (TER-40): a reload after an
  // inbox event keeps the object, and so the DOM and the local state of
  // each card that stays (a typed answer, a chosen option, an open reply,
  // the focus). Only new and gone items change the DOM.
  const [items, setItems] = createStore<{ list: InboxItem[] }>({ list: [] });
  const list = () => items.list;
  const [visibleCount, setVisibleCount] = createSignal(PAGE_SIZE);
  const [hasMore, setHasMore] = createSignal(false);
  let cursor: string | undefined;
  const visible = createMemo(() => list().slice(0, visibleCount()));
  const setList = (next: InboxItem[] | ((cur: InboxItem[]) => InboxItem[])) => {
    const rows = typeof next === "function" ? next(items.list) : next;
    // Keep every mounted card mounted when a new item is inserted above it.
    const mounted = new Set(visible().map((i) => i.id));
    const last = rows.reduce((n, i, index) => mounted.has(i.id) ? index + 1 : n, 0);
    setVisibleCount((n) => Math.max(n, last));
    keepView(() => setItems("list", reconcile(rows, { key: "id", merge: true })));
  };
  let listEl: HTMLDivElement | undefined;
  /**
   * Run `change`, and keep the card that holds the focus (else the first
   * card in view) at the same place on the screen: a new item above it
   * must not move what the operator works on.
   */
  const keepView = (change: () => void) => {
    const anchor = anchorCard(listEl);
    const before = anchor?.getBoundingClientRect().top;
    change();
    if (!anchor || before === undefined) return;
    const after = anchor.getBoundingClientRect().top;
    if (anchor.isConnected && after !== before) window.scrollBy(0, after - before);
  };
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  let gen = 0;
  const reload = async (append = false) => {
    const mine = ++gen;
    setLoading(true);
    try {
      const boundary = append ? undefined : list().at(-1);
      const target = params.item;
      let nextCursor = append ? cursor : undefined;
      const rows: InboxItem[] = [];
      let more = false;
      do {
        const page = await api.inbox.list({ ...inboxQuery(tab(), status(), project()), limit: FETCH_SIZE, cursor: nextCursor });
        if (mine !== gen) return;
        rows.push(...page);
        const last = page.at(-1);
        const next = last ? cursorOf(last) : undefined;
        more = page.length === FETCH_SIZE && next !== nextCursor;
        nextCursor = next;
        if (!more) break;
        if (target && !rows.some((i) => i.id === target)) continue;
        if (boundary && !rows.some((i) => byInboxOrder(i, boundary) >= 0)) continue;
        if (rows.filter(keep).length >= PAGE_SIZE) break;
      } while (more);
      cursor = nextCursor;
      setHasMore(more);
      // The daemon's `open` has the waiting asks too: they show under Asked only.
      setList(sortItems((append ? [...list(), ...rows] : rows).filter(keep)));
      if (append) setVisibleCount((n) => n + PAGE_SIZE);
      setError(null);
    } catch (e) {
      if (mine === gen) setError(e);
    } finally {
      if (mine === gen) setLoading(false);
    }
  };
  createEffect(on([tab, status, project], () => {
    setItems("list", []);
    cursor = undefined;
    setHasMore(false);
    setVisibleCount(PAGE_SIZE);
    void reload();
  }));

  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (!isInboxEvent(e)) return;
      clearTimeout(t);
      t = setTimeout(() => void reload(), 300);
    });
    onCleanup(() => (off(), clearTimeout(t)));
  });

  // `?item=<id>` (the critical banner's link): scroll to the item once it is in the list, and highlight it.
  let scrolledTo: string | null = null;
  createEffect(() => {
    const target = params.item;
    if (!target || target === scrolledTo) return;
    const index = list().findIndex((i) => i.id === target);
    if (index < 0) return;
    // A critical banner or question link must still reach an item beyond the first page.
    setVisibleCount((n) => Math.max(n, index + 1));
    scrolledTo = target;
    requestAnimationFrame(() => document.getElementById(inboxItemDomId(target))?.scrollIntoView({ block: "center", behavior: "smooth" }));
  });

  /** Put the changed item in at once; the badge reads the counts again. */
  const changed = (item: InboxItem) => {
    setList((xs) => putItem(xs, item, keep));
    void inbox.refresh();
  };
  const questionChanged = (item: InboxItem, q: Question) => {
    changed({ ...item, question: q, ...questionFlags(q) });
  };
  const mailAcked = (item: InboxItem, m: Message) => changed({ ...item, message: m, open: false, urgent: false, critical: false });
  const systemChanged = () => (void reload(), void inbox.refresh());
  const watchChanged = (item: InboxItem, w: InboxWatch) => changed({ ...item, watch: w, open: w.waiting && !w.cancelled_at });

  // ----- ask and forward (GitHub #24) -----
  /** The open dialog: an ask (no question), or a forward of a question. */
  const [asking, setAsking] = createSignal<{ question?: Question } | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  /** "Message an agent" (TER-46): the dialog, and the last mail sent from it. */
  const [messaging, setMessaging] = createSignal(false);
  const navigate = useNavigate();
  const messaged = (m: { id: string }) => {
    setMessaging(false);
    navigate(`/threads/${m.id}`);
  };
  const asked = (agent: string, forward: boolean) => {
    setAsking(null);
    setNotice(`${forward ? "Forwarded" : "Asked"}. The Asked tab shows the thread until ${agent} answers.`);
    setTimeout(() => setNotice(null), 6000);
    void reload();
    void inbox.refresh();
  };
  /** Text for the answer box of a question ("Use as answer"), by question ID. */
  const [prefill, setPrefill] = createSignal<Record<string, { text: string }>>({});
  /** Show the question of a forward: the All tab, its status, and `?item=` scrolls to it. */
  const openQuestion = async (id: string) => {
    try {
      const q = await api.questions.get(id);
      setParams({ tab: undefined, status: q.status === "open" ? undefined : "done", item: questionItemId(q) });
    } catch (e) {
      setError(e);
    }
  };
  const useAsAnswer = (id: string, text: string) => {
    setPrefill((p) => ({ ...p, [id]: { text } }));
    void openQuestion(id);
  };

  return (
    <div class="stack">
      <PageHead title="Inbox">
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
        <button onClick={() => setMessaging(true)} title="Send one mail to an agent; nothing waits for an answer">
          Message an agent
        </button>
        <button class="primary" onClick={() => setAsking({})}>
          Ask an agent
        </button>
      </PageHead>
      <p class="muted small" style={{ margin: 0 }}>
        Everything that waits for you. Leader agents ask questions and send notices with <code>terrarium question</code>. Agents and the daemon send you
        mail. The loop guard pauses a thread when agents write too many messages to each other. Ask an agent, or forward a question to one: the Asked tab
        waits for its answer. Your conversations with agents are on the <A href="/threads">Threads</A> screen. The text of each item is the sender's own words.
      </p>
      <Show when={status() !== "asked"}>
        <Tabs tabs={INBOX_TABS} active={tab()} onChange={(t) => setParams({ tab: t === "all" ? undefined : t })} />
      </Show>
      <Tabs tabs={STATUS_TABS} active={status()} onChange={(s) => setParams({ status: s === "open" ? undefined : s, tab: params.tab === "watch" ? undefined : params.tab })} />
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={notice()}>
        <p class="notice small" role="status" style={{ margin: 0 }}>
          {notice()}
        </p>
      </Show>
      <Show when={list().length} fallback={<Empty>{loading() ? "Loading…" : emptyText(tab(), status())}</Empty>}>
        <div class="stack" ref={listEl}>
          <For each={visible()}>
            {(item) => (
              <div id={inboxItemDomId(item.id)} classList={{ "inbox-target": params.item === item.id }}>
                <Switch>
                  <Match when={item.question}>
                    {(q) => {
                      const forward = () => (
                        <Show when={canForward(q())}>
                          <button type="button" class="small" onClick={() => setAsking({ question: q() })} title="Forward to an agent, and watch for its answer">
                            <EyeIcon /> Forward
                          </button>
                        </Show>
                      );
                      return (
                        <Show
                          when={q().kind === "bundle"}
                          fallback={<QuestionCard question={q()} now={now()} showAgent showKind actions={forward()} prefill={prefill()[q().id]} onChange={(next) => questionChanged(item, next)} />}
                        >
                          <BundleCard question={q()} now={now()} showAgent showKind actions={forward()} onChange={(next) => questionChanged(item, next)} />
                        </Show>
                      );
                    }}
                  </Match>
                  <Match when={item.message}>
                    {(m) => <MailItemCard message={m()} open={item.open} now={now()} watch={item.watch} onOpenQuestion={(id) => void openQuestion(id)} onUseAsAnswer={useAsAnswer} onAck={(next) => mailAcked(item, next)} />}
                  </Match>
                  <Match when={item.system}>{(s) => <PausedThreadCard system={s()} onChange={systemChanged} />}</Match>
                  <Match when={item.kind === "watch" && item.watch}>{(w) => <WatchCard watch={w()} now={now()} onChange={(next) => watchChanged(item, next)} />}</Match>
                </Switch>
              </div>
            )}
          </For>
        </div>
        <Show when={visibleCount() < list().length || hasMore()}>
          <button disabled={loading()} onClick={() => visibleCount() < list().length ? setVisibleCount((n) => n + PAGE_SIZE) : void reload(true)}>
            Load more
          </button>
        </Show>
      </Show>
      <Show when={messaging()}>
        <MessageDialog onClose={() => setMessaging(false)} onSent={messaged} />
      </Show>
      <Show when={asking()}>
        {(a) => <AskDialog question={a().question} onClose={() => setAsking(null)} onAsked={(r) => asked(r.watch.agent_name, !!a().question)} onRequested={(t) => { setAsking(null); navigate(`/threads/${t.id}`); }} />}
      </Show>
    </div>
  );
}

/** The watch icon of Forward: an eye. */
function EyeIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true" style={{ "vertical-align": "-2px" }}>
      <path d="M1 12s4-7 11-7 11 7 11 7-4 7-11 7S1 12 1 12z" />
      <circle cx="12" cy="12" r="3" />
    </svg>
  );
}

/** The card to keep in place: the one that holds the focus, else the first one whose top is in view. */
function anchorCard(listEl: HTMLElement | undefined): HTMLElement | undefined {
  if (!listEl) return undefined;
  const cards = [...listEl.children] as HTMLElement[];
  const focused = document.activeElement;
  const withFocus = focused ? cards.find((c) => c.contains(focused)) : undefined;
  return withFocus ?? cards.find((c) => c.getBoundingClientRect().bottom > 0);
}

