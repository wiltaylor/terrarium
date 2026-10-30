// The Inbox: everything that waits for the operator (`GET /api/inbox`).
// Questions, bundles of questions, and notices of the leaders, mail to `operator`, and threads that
// the loop guard paused. Tabs pick the kind and Open or Done; the project
// filter picks one project. `inbox` and `question` events read the list again.

import { useSearchParams } from "@solidjs/router";
import { createEffect, createSignal, For, Match, on, onCleanup, onMount, Show, Switch } from "solid-js";
import type { InboxItem, Message, Question } from "../../api/types";
import { Empty, ErrorBanner, PageHead, Tabs, useNow } from "../../components/ui";
import { api, events, inbox, projects } from "../../stores";
import { MailItemCard, PausedThreadCard } from "./cards";
import { emptyText, INBOX_TABS, inboxQuery, isInboxEvent, matchesTabs, putItem, questionFlags, sortItems, STATUS_TABS, type InboxStatusTab, type InboxTab } from "./model";
import { inboxItemDomId } from "./critical";
import { BundleCard } from "./BundleCard";
import { QuestionCard } from "./QuestionCard";

const TAB_IDS = INBOX_TABS.map((t) => t.id) as string[];

export default function InboxPage() {
  const now = useNow(30_000);
  const [params, setParams] = useSearchParams<{ tab?: string; status?: string; project?: string; item?: string }>();
  const tab = (): InboxTab => (TAB_IDS.includes(params.tab ?? "") ? (params.tab as InboxTab) : "all");
  const status = (): InboxStatusTab => (params.status === "done" ? "done" : "open");
  const project = () => params.project ?? "";
  const keep = (i: InboxItem) => matchesTabs(i, tab(), status(), project());

  const [list, setList] = createSignal<InboxItem[]>([]);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  let gen = 0;
  const reload = async () => {
    const mine = ++gen;
    setLoading(true);
    try {
      const rows = await api.inbox.list(inboxQuery(tab(), status(), project()));
      if (mine !== gen) return;
      setList(sortItems(rows));
      setError(null);
    } catch (e) {
      if (mine === gen) setError(e);
    } finally {
      if (mine === gen) setLoading(false);
    }
  };
  createEffect(on([tab, status, project], () => void reload()));

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
    if (!target || target === scrolledTo || !list().some((i) => i.id === target)) return;
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
      </PageHead>
      <p class="muted small" style={{ margin: 0 }}>
        Everything that waits for you. Leader agents ask questions and send notices with <code>terrarium question</code>. Agents and the daemon send you
        mail. The loop guard pauses a thread when agents write too many messages to each other. The text of each item is the sender's own words.
      </p>
      <Tabs tabs={INBOX_TABS} active={tab()} onChange={(t) => setParams({ tab: t === "all" ? undefined : t })} />
      <Tabs tabs={STATUS_TABS} active={status()} onChange={(s) => setParams({ status: s === "open" ? undefined : s })} />
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={list().length} fallback={<Empty>{loading() ? "Loading…" : emptyText(tab(), status())}</Empty>}>
        <div class="stack">
          <For each={list()}>
            {(item) => (
              <div id={inboxItemDomId(item.id)} classList={{ "inbox-target": params.item === item.id }}>
                <Switch>
                  <Match when={item.question}>
                    {(q) => (
                      <Show when={q().kind === "bundle"} fallback={<QuestionCard question={q()} now={now()} showAgent showKind onChange={(next) => questionChanged(item, next)} />}>
                        <BundleCard question={q()} now={now()} showAgent showKind onChange={(next) => questionChanged(item, next)} />
                      </Show>
                    )}
                  </Match>
                  <Match when={item.message}>{(m) => <MailItemCard message={m()} open={item.open} now={now()} onAck={(next) => mailAcked(item, next)} />}</Match>
                  <Match when={item.system}>{(s) => <PausedThreadCard system={s()} onChange={systemChanged} />}</Match>
                </Switch>
              </div>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}
