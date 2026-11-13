// The Repos, Knowledge and Logs widget parts of project pages (TER-1076, C6).
// Each part renders an existing project-scoped panel for the host project of
// the page. C7 composes this renderer into the page host and turns it on.
//
// Typed options (docs/api.md, project pages): `view` selects the part, and
// `search` seeds the panel's own local filter or selector:
//   repos:files         the repo name to open first
//   wiki_index:browser  the page-tree search
//   memory:list|graph   the memory search
//   db:records          a DB query; its `type:` term selects the type
//   mail:messages       the free-text filter
// repos:list, db:types and events:events have no text filter and ignore it.
// A configured value seeds the panel once and replaces the local value only
// when the configured value changes, so a layout refresh keeps local state.
//
// A widget of another kind, an unknown view or invalid options shows Not
// Found and loads nothing. A copied reference (repo name, DB type) that the
// host project lacks shows Not Found: no other project is ever asked.
// Domain actions keep their existing controls and server rights checks.

import { createMemo, Show, type Component } from "solid-js";
import type { PageWidget, ProjectWidgetViews } from "../../../api/types";
import { DbRecordsPanel } from "../../db/DbRecordsPage";
import { DbTypesPanel } from "../../db/DbTypesPage";
import { EventsPanel } from "../../events/EventsPage";
import { MailPanel } from "../../mail/MailPage";
import { MemoryPanel } from "../../memory/MemoryPage";
import { RepoFilesPanel } from "../../repos/RepoFilesPanel";
import { ReposPanel } from "../../repos/ReposPanel";
import { WikiBrowserPanel } from "../../wiki/WikiPage";
import { validFeatureOptions, widgetDefinition } from "../widgetRegistry";

export const KNOWLEDGE_LOGS_REPOS_KINDS = ["repos", "wiki_index", "memory", "db", "mail", "events"] as const;
export type KnowledgeLogsReposKind = (typeof KNOWLEDGE_LOGS_REPOS_KINDS)[number];
export type KnowledgeLogsReposPart = { [K in KnowledgeLogsReposKind]: `${K}:${ProjectWidgetViews[K]}` }[KnowledgeLogsReposKind];

/** What a part receives: the host project only, never a project from options. */
export interface FeaturePartProps { project: string; search?: string }

export const KNOWLEDGE_LOGS_REPOS_PARTS: { readonly [P in KnowledgeLogsReposPart]: Component<FeaturePartProps> } = {
  "repos:list": (props) => <ReposPanel project={props.project} />,
  "repos:files": (props) => <RepoFilesPanel project={props.project} search={props.search} />,
  "wiki_index:browser": (props) => <WikiBrowserPanel project={props.project} search={props.search} />,
  "memory:list": (props) => <MemoryPanel project={props.project} view="list" search={props.search} />,
  "memory:graph": (props) => <MemoryPanel project={props.project} view="graph" search={props.search} />,
  "db:types": (props) => <DbTypesPanel project={props.project} />,
  "db:records": (props) => <DbRecordsPanel project={props.project} search={props.search} />,
  "mail:messages": (props) => <MailPanel project={props.project} search={props.search} />,
  "events:events": (props) => <EventsPanel project={props.project} />,
};

const isOwnKind = (kind: PageWidget["kind"]): kind is KnowledgeLogsReposKind => (KNOWLEDGE_LOGS_REPOS_KINDS as readonly string[]).includes(kind);

/** The part of a widget, or undefined when it is not a valid part of this module. */
export function knowledgeLogsReposPart(widget: Pick<PageWidget, "kind" | "options">): KnowledgeLogsReposPart | undefined {
  if (!isOwnKind(widget.kind) || !validFeatureOptions(widget.kind, widget.options)) return undefined;
  const part = widgetDefinition(widget)?.part;
  return part && Object.hasOwn(KNOWLEDGE_LOGS_REPOS_PARTS, part) ? (part as KnowledgeLogsReposPart) : undefined;
}

/**
 * One widget instance. The panel mounts again only when its part or the host
 * project changes, never for a new widget object of the same part (a layout
 * refresh, a move, a title edit).
 */
export function KnowledgeLogsReposWidget(props: { project: string; widget: PageWidget }) {
  const part = createMemo(() => knowledgeLogsReposPart(props.widget));
  const search = createMemo(() => props.widget.options?.search);
  return (
    <Show when={part()} keyed fallback={<p>Not Found</p>}>
      {(current) => {
        const Part = KNOWLEDGE_LOGS_REPOS_PARTS[current];
        return <Part project={props.project} search={search()} />;
      }}
    </Show>
  );
}
