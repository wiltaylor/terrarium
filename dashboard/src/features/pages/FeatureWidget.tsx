// The final host dispatch of the feature widgets of project pages
// (TER-1077, C7). It composes the renderer maps of C4 (Board, Tests,
// Agents), C5 (Schedules, Rules, Pipelines, Calendar) and C6 (Repos,
// Knowledge, Logs). A kind of no map, an unknown view or invalid options
// show Not Found and load nothing. Every part gets the page's project only.

import { createMemo, Show, type Component } from "solid-js";
import { Dynamic } from "solid-js/web";
import type { PageWidget, ProjectWidgetKind } from "../../api/types";
import { BOARD_TESTS_AGENTS_PARTS, boardTestsAgentsPart, type BoardTestsAgentsKind, type FeaturePartProps } from "./parts/boardTestsAgents";
import { KnowledgeLogsReposWidget, KNOWLEDGE_LOGS_REPOS_KINDS } from "./parts/knowledgeLogsRepos";
import { OPERATIONS_WIDGET_RENDERERS, OperationsWidget } from "./parts/operations";
import { validFeatureOptions } from "./widgetRegistry";
import type { PageGrants } from "./pageGrants";

export type FeatureFamily = "boardTestsAgents" | "operations" | "knowledgeLogsRepos";

/** The map that renders a kind, or undefined when no map has it. */
export function featureFamily(kind: PageWidget["kind"]): FeatureFamily | undefined {
  if (Object.hasOwn(BOARD_TESTS_AGENTS_PARTS, kind)) return "boardTestsAgents";
  if (Object.hasOwn(OPERATIONS_WIDGET_RENDERERS, kind)) return "operations";
  if ((KNOWLEDGE_LOGS_REPOS_KINDS as readonly string[]).includes(kind)) return "knowledgeLogsRepos";
  return undefined;
}

function BoardTestsAgentsWidget(props: { project: string; widget: PageWidget; grants: PageGrants }) {
  const kind = () => props.widget.kind as BoardTestsAgentsKind;
  // A new widget object of the same kind and view keeps the mounted part.
  const part = createMemo(() => validFeatureOptions(kind(), props.widget.options) ? boardTestsAgentsPart(kind(), props.widget.options) : undefined);
  return <Show when={part()} keyed fallback={<p>Not Found</p>}>
    {current => <Dynamic component={current as Component<FeaturePartProps<BoardTestsAgentsKind>>} project={props.project}
      widget={props.widget as FeaturePartProps<BoardTestsAgentsKind>["widget"]} grants={props.grants.boardTestsAgents} />}
  </Show>;
}

/** One feature widget of a page. `widget` and `grants` are read reactively. */
export function FeatureWidget(props: { project: string; widget: PageWidget; grants: PageGrants }) {
  const family = createMemo(() => featureFamily(props.widget.kind as ProjectWidgetKind));
  return <Show when={family()} keyed fallback={<p>Not Found</p>}>
    {current => current === "boardTestsAgents" ? <BoardTestsAgentsWidget project={props.project} widget={props.widget} grants={props.grants} />
      : current === "operations" ? <Show when={validFeatureOptions(props.widget.kind as ProjectWidgetKind, props.widget.options)} fallback={<p>Not Found</p>}>
        <OperationsWidget project={props.project} widget={props.widget} grants={props.grants.operations} />
      </Show>
      : <KnowledgeLogsReposWidget project={props.project} widget={props.widget} grants={props.grants.knowledgeLogsRepos} />}
  </Show>;
}
