import { useInstance } from "../../instance";
// The questions, bundles, and notices linked to one card, in the card
// drawer (docs/api.md §6.8 `GET /api/questions?card=`). The same components
// as the Inbox, so the operator answers, acknowledges, or retracts here. The
// closed ones are under "Answered". The operator links an open question of
// the project that has no card, or unlinks one (`PATCH` with only `card`).

import { createEffect, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import type { Card, Question } from "../../api/types";
import { ErrorBanner, useNow } from "../../components/ui";

import { BundleCard } from "../inbox/BundleCard";
import { QuestionCard } from "../inbox/QuestionCard";
import { putQuestion, shortQuestion } from "../inbox/questions";
import { isCardQuestionEvent, splitCardQuestions } from "./questions";

export function CardQuestions(props: { card: Card; linkable: Question[]; onChanged: () => void }) {
  const { api, events, inbox } = useInstance();
  const now = useNow(30_000);
  const [list, setList] = createSignal<Question[]>([]);
  const [loaded, setLoaded] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const [pick, setPick] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  let gen = 0;
  const reload = async () => {
    const mine = ++gen;
    try {
      const rows = await api.questions.list({ card: props.card.id, status: "all" });
      if (mine !== gen) return;
      setList(rows);
      setError(null);
    } catch (e) {
      if (mine === gen) setError(e);
    } finally {
      if (mine === gen) setLoaded(true);
    }
  };
  createEffect(
    on(
      () => props.card.id,
      () => {
        setList([]);
        setLoaded(false);
        void reload();
      },
    ),
  );
  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (!isCardQuestionEvent(e, { card: props.card.id })) return;
      clearTimeout(t);
      t = setTimeout(() => void reload(), 300);
    });
    onCleanup(() => (off(), clearTimeout(t)));
  });

  // An answer, an acknowledge, a retract, or a change of the card: the list, the Inbox counts, and the board badges.
  const change = (q: Question) => {
    setList((xs) => (q.card === props.card.id ? putQuestion(xs, q) : xs.filter((x) => x.id !== q.id)));
    setError(null);
    void inbox.refresh();
    props.onChanged();
  };
  const setCard = async (id: string, card: string | null) => {
    setBusy(true);
    try {
      change(await api.questions.setCard(id, card));
      return true;
    } catch (e) {
      setError(e);
      return false;
    } finally {
      setBusy(false);
    }
  };

  const groups = () => splitCardQuestions(list());
  const closedLabel = () => {
    const c = groups().closed;
    return c.some((q) => q.status === "retracted") ? `Answered or retracted (${c.length})` : `Answered (${c.length})`;
  };
  const unlink = (q: Question) => (
    <Show when={q.thread || (q.kind !== "question" && q.kind !== "bundle")}>
      <button type="button" class="small" disabled={busy()} title={`Unlink ${q.kind} ${q.id} from ${props.card.id}. It stays in the Inbox.`} onClick={() => void setCard(q.id, null)}>
        Unlink
      </button>
    </Show>
  );
  const view = (q: Question) => (
    <Show when={q.kind === "bundle"} fallback={<QuestionCard question={q} now={now()} showAgent showKind hideCard actions={unlink(q)} onChange={change} />}>
      <BundleCard question={q} now={now()} showAgent showKind hideCard actions={unlink(q)} onChange={change} />
    </Show>
  );

  return (
    <section class="stack" style={{ gap: "8px" }} aria-label="Questions">
      <h3 style={{ margin: 0 }}>Questions</h3>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <For each={groups().open} fallback={<p class="muted small">{loaded() ? "No open questions on this card." : "Loading…"}</p>}>
        {(q) => view(q)}
      </For>
      <Show when={groups().closed.length}>
        <details>
          <summary>{closedLabel()}</summary>
          <div class="stack" style={{ gap: "8px", "margin-top": "8px" }}>
            <For each={groups().closed}>{(q) => view(q)}</For>
          </div>
        </details>
      </Show>
      <Show when={props.linkable.length}>
        <form
          style={{ display: "grid", "grid-template-columns": "minmax(0, 1fr) auto", gap: "6px", "align-items": "center" }}
          aria-label="Link a question"
          onSubmit={async (e) => {
            e.preventDefault();
            if (pick() && (await setCard(pick(), props.card.id))) setPick("");
          }}
        >
          <select value={pick()} onChange={(e) => setPick(e.currentTarget.value)} aria-label="An open question of the project with no card" style={{ width: "100%", "min-width": 0 }}>
            <option value="">Link a question…</option>
            <For each={props.linkable}>
              {(q) => (
                <option value={q.id}>
                  {q.kind}: {shortQuestion(q.text, 70)} ({q.agent_name})
                </option>
              )}
            </For>
          </select>
          <button class="small" type="submit" disabled={!pick() || busy()}>
            Link
          </button>
        </form>
      </Show>
    </section>
  );
}
