// The questions and notices of one agent, on the agent page. Only a leader
// asks or sends a notice, so the panel shows for a leader, or when the agent
// has items. `question` events of the agent read the list again.

import { createEffect, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import type { Agent, Question } from "../../api/types";
import { Empty, ErrorBanner, useNow } from "../../components/ui";
import { api, events, inbox } from "../../stores";
import { QuestionCard } from "./QuestionCard";
import { isQuestionEvent, putQuestion, sortQuestions } from "./questions";

export function QuestionsPanel(props: { agent: Agent }) {
  const now = useNow(30_000);
  const [all, setAll] = createSignal(false);
  const [list, setList] = createSignal<Question[]>([]);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  let gen = 0;
  const reload = async () => {
    const mine = ++gen;
    setLoading(true);
    try {
      const rows = await api.questions.ofAgent(props.agent.id, { all: all() });
      if (mine !== gen) return;
      setList(sortQuestions(rows));
      setError(null);
    } catch (e) {
      if (mine === gen) setError(e);
    } finally {
      if (mine === gen) setLoading(false);
    }
  };
  createEffect(on([() => props.agent.id, all], () => void reload()));
  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (!isQuestionEvent(e, props.agent.id)) return;
      clearTimeout(t);
      t = setTimeout(() => void reload(), 300);
    });
    onCleanup(() => (off(), clearTimeout(t)));
  });
  const change = (q: Question) => {
    setList((xs) => putQuestion(xs, q).filter((x) => all() || x.status !== "retracted"));
    setError(null);
    void inbox.refresh();
  };
  const leader = () => props.agent.roles.some((r) => r === "project_admin" || r === "sysadmin");

  return (
    <Show when={leader() || list().length}>
      <section class="card-box stack" style={{ gap: "8px" }} aria-label="Questions and notices">
        <div class="spread">
          <h3 style={{ margin: 0 }}>Questions and notices to the operator</h3>
          <label class="row small">
            <input type="checkbox" checked={all()} onChange={(e) => setAll(e.currentTarget.checked)} />
            all
          </label>
        </div>
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <Show when={list().length} fallback={<Empty>{loading() ? "Loading…" : "No open or recent questions or notices."}</Empty>}>
          <For each={list()}>{(x) => <QuestionCard question={x} now={now()} showKind onChange={change} />}</For>
        </Show>
      </section>
    </Show>
  );
}
