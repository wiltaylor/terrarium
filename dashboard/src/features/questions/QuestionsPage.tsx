// The Questions page: the questions of the leaders to the operator, urgent
// first (docs/api.md §6.8). Each card answers or retracts one. A `question`
// event reads the list again. The agent page shows the questions of one
// agent with `QuestionsPanel`.

import { createEffect, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import type { Agent, Question } from "../../api/types";
import { Empty, ErrorBanner, PageHead, Tabs, useNow } from "../../components/ui";
import { api, events, projects, questions } from "../../stores";
import { QuestionCard } from "./QuestionCard";
import { isQuestionEvent, matchesFilter, putQuestion, QUESTION_FILTERS, sortQuestions, type QuestionFilter } from "./model";

/** Read again soon after an event. Many events in a burst give one request. */
function useSoon(fn: () => void, ms = 300) {
  let t: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(t));
  return () => {
    clearTimeout(t);
    t = setTimeout(fn, ms);
  };
}

/** A list of questions that a loader fills and `question` events keep live. */
function useQuestions(load: () => Promise<Question[]>, keep: () => (q: Question) => boolean, agentId?: () => string) {
  const [list, setList] = createSignal<Question[]>([]);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  let gen = 0;
  const reload = async () => {
    const mine = ++gen;
    setLoading(true);
    try {
      const rows = await load();
      if (mine !== gen) return;
      setList(sortQuestions(rows));
      setError(null);
    } catch (e) {
      if (mine === gen) setError(e);
    } finally {
      if (mine === gen) setLoading(false);
    }
  };
  const soon = useSoon(() => void reload());
  onMount(() => onCleanup(events.on((e) => isQuestionEvent(e, agentId?.()) && soon())));
  const change = (q: Question) => {
    setList((xs) => putQuestion(xs, q).filter(keep()));
    setError(null);
    void questions.refresh();
  };
  return { list, loading, error, setError, reload, change };
}

export default function QuestionsPage() {
  const now = useNow(30_000);
  const [filter, setFilter] = createSignal<QuestionFilter>("open");
  const [project, setProject] = createSignal("");
  const q = useQuestions(
    () => api.questions.list({ status: filter(), ...(project() ? { project: project() } : {}) }),
    () => (x) => matchesFilter(x, filter()),
  );
  createEffect(on([filter, project], () => void q.reload()));

  return (
    <div class="stack">
      <PageHead title="Questions">
        <select aria-label="Project" value={project()} onChange={(e) => setProject(e.currentTarget.value)}>
          <option value="">All projects</option>
          <For each={projects.projects()}>{(p) => <option value={p.name}>{p.name}</option>}</For>
        </select>
      </PageHead>
      <p class="muted small" style={{ margin: 0 }}>
        Leader agents ask you here with <code>terrarium question ask</code>. Your answer goes to the agent by mail and wakes it. The question text and the
        options are the agent's own words.
      </p>
      <Tabs tabs={QUESTION_FILTERS} active={filter()} onChange={setFilter} />
      <ErrorBanner error={q.error()} onDismiss={() => q.setError(null)} />
      <Show when={q.list().length} fallback={<Empty>{q.loading() ? "Loading…" : filter() === "open" ? "No open questions." : "No questions."}</Empty>}>
        <div class="stack">
          <For each={q.list()}>{(x) => <QuestionCard question={x} now={now()} showAgent onChange={q.change} />}</For>
        </div>
      </Show>
    </div>
  );
}

/** The questions of one agent, on the agent page. Only a leader asks, so the panel shows for leaders, or when there are questions. */
export function QuestionsPanel(props: { agent: Agent }) {
  const now = useNow(30_000);
  const [all, setAll] = createSignal(false);
  const q = useQuestions(
    () => api.questions.ofAgent(props.agent.id, { all: all() }),
    () => (x) => all() || x.status !== "retracted",
    () => props.agent.id,
  );
  createEffect(on([() => props.agent.id, all], () => void q.reload()));
  const leader = () => props.agent.roles.some((r) => r === "project_admin" || r === "sysadmin");

  return (
    <Show when={leader() || q.list().length}>
      <section class="card-box stack" style={{ gap: "8px" }} aria-label="Questions">
        <div class="spread">
          <h3 style={{ margin: 0 }}>Questions to the operator</h3>
          <label class="row small">
            <input type="checkbox" checked={all()} onChange={(e) => setAll(e.currentTarget.checked)} />
            all
          </label>
        </div>
        <ErrorBanner error={q.error()} onDismiss={() => q.setError(null)} />
        <Show when={q.list().length} fallback={<Empty>{q.loading() ? "Loading…" : "No open or recent questions."}</Empty>}>
          <For each={q.list()}>{(x) => <QuestionCard question={x} now={now()} onChange={q.change} />}</For>
        </Show>
      </section>
    </Show>
  );
}
