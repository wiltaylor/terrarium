// The Board, Tests and Agents parts of project pages (TER-1074, docs/api.md
// "Project pages"). Each view of the registry (widgetRegistry.ts
// PROJECT_WIDGET_VIEWS) has one renderer here. A renderer wraps the panel of
// the route page and keeps its query state in the widget instance, not in the
// URL, so duplicate widgets stay independent. The route pages are unchanged.
//
// CustomPage does not dispatch to these maps yet. TER-1077 (C7) composes them
// into the host and turns the project_pages_v2 capability on. Until then
// nothing here is visible.

import { type Component, createEffect, createResource, createSignal, on, onCleanup, Show } from "solid-js";
import type { Board, Column, PageWidget, ProjectWidgetKind, ProjectWidgetViews } from "../../../api/types";
import { ErrorBanner } from "../../../components/ui";
import { useInstance } from "../../../instance";
import { AgentsPanel, CardAgentsPanel } from "../../agents/AgentsPanel";
import { BoardPanel, type BoardQuery } from "../../board/BoardPanel";
import { ColumnsEditorFields } from "../../board/ColumnsEditor";
import { TestRunsPanel, type TestRunsQuery } from "../../tests/TestRunsPanel";
import { patchQuery, widgetSearch, widgetView } from "./localQuery";

/** The props of one widget part. `widget` is read reactively, so a saved option change applies in place. */
export interface FeaturePartProps<K extends ProjectWidgetKind> {
  project: string;
  widget: Pick<PageWidget, "id" | "options"> & { kind: K };
}
/** One renderer for each view of a kind. */
export type FeaturePartMap<K extends ProjectWidgetKind> = { readonly [V in ProjectWidgetViews[K]]: Component<FeaturePartProps<K>> };

/** The lanes and the done cards: the board panel with a widget-local query and done toggle. */
function BoardCards(props: FeaturePartProps<"board"> & { done: boolean }) {
  const [query, setQuery] = createSignal<BoardQuery>({});
  const [done, setDone] = createSignal(props.done);
  // A saved view change (lanes or done) and a project change start from a clean query.
  createEffect(on(() => [props.project, props.done] as const, ([, saved]) => { setQuery({}); setDone(saved); }, { defer: true }));
  return <BoardPanel project={props.project} done={done()} query={query()}
    onQueryChange={patch => setQuery(q => patchQuery(q, patch))}
    onToggleDone={() => { setQuery(q => patchQuery(q, { card: undefined, col: undefined })); setDone(d => !d); }} />;
}

/** Columns that a board event reloads in the background. The editor keeps its draft until Save or Cancel. */
const COLUMNS_RELOAD_MS = 300;
const sameColumns = (a: Column[] | undefined, b: Column[] | undefined) => JSON.stringify(a) === JSON.stringify(b);

function BoardColumns(props: FeaturePartProps<"board">) {
  return <Show when={props.project} keyed>{project => <BoardColumnsContent project={project} />}</Show>;
}
function BoardColumnsContent(props: { project: string }) {
  const { api, events } = useInstance();
  const [board, { refetch, mutate }] = createResource(() => props.project, p => api.board.get(p));
  // The columns that the open editor started from. A new value remounts the editor.
  const [base, setBase] = createSignal<Column[]>();
  createEffect(on(() => board.latest, b => { if (b && !base()) setBase(b.columns); }));
  const reset = () => setBase(board.latest?.columns);
  let timer: ReturnType<typeof setTimeout> | undefined;
  const off = events.on(e => {
    if (e.project !== props.project || (e.kind !== "board" && e.kind !== "card")) return;
    clearTimeout(timer);
    timer = setTimeout(() => void refetch(), COLUMNS_RELOAD_MS);
  });
  onCleanup(() => { clearTimeout(timer); off(); });
  const stale = () => !!base() && !!board.latest && !sameColumns(base(), board.latest.columns);
  return <div class="columns-editor-widget">
    <ErrorBanner error={board.error} />
    <Show when={stale()}><p class="muted small" role="status">The columns changed on the server. Cancel to load them.</p></Show>
    <Show when={base()} keyed fallback={<p class="muted">{board.loading ? "Loading…" : ""}</p>}>
      {columns => <ColumnsEditorFields columns={columns} onClose={reset} onSave={async (edits): Promise<Board> => {
        const saved = await api.board.update(props.project, edits);
        mutate(saved);
        return saved;
      }} />}
    </Show>
  </div>;
}

export const BOARD_PARTS: FeaturePartMap<"board"> = {
  lanes: props => <BoardCards {...props} done={false} />,
  done: props => <BoardCards {...props} done />,
  columns: BoardColumns,
};

/** The test runs with a widget-local query. The saved search is the first name filter. */
function TestRuns(props: FeaturePartProps<"tests">) {
  const initial = (): TestRunsQuery => patchQuery({}, { name: widgetSearch(props.widget.options) });
  const [query, setQuery] = createSignal<TestRunsQuery>(initial());
  createEffect(on(() => [props.project, widgetSearch(props.widget.options)] as const, () => setQuery(initial()), { defer: true }));
  return <TestRunsPanel project={props.project} query={query()} onQueryChange={patch => setQuery(q => patchQuery(q, patch))} />;
}

export const TESTS_PARTS: FeaturePartMap<"tests"> = { runs: TestRuns };

export const AGENTS_PARTS: FeaturePartMap<"agents"> = {
  agents: props => <AgentsPanel project={props.project} />,
  card_agents: props => <CardAgentsPanel project={props.project} empty="No card agents." />,
};

export type BoardTestsAgentsKind = "board" | "tests" | "agents";
export const BOARD_TESTS_AGENTS_PARTS: { readonly [K in BoardTestsAgentsKind]: FeaturePartMap<K> } = {
  board: BOARD_PARTS, tests: TESTS_PARTS, agents: AGENTS_PARTS,
};

/** The renderer of a Board, Tests or Agents widget, or undefined for an unknown view (the host shows Not Found). */
export function boardTestsAgentsPart<K extends BoardTestsAgentsKind>(kind: K, options: PageWidget["options"]): Component<FeaturePartProps<K>> | undefined {
  const view = widgetView(kind, options);
  if (view === undefined) return undefined;
  return (BOARD_TESTS_AGENTS_PARTS[kind] as FeaturePartMap<K>)[view];
}
