import type { PageWidget, ProjectWidgetKind, ProjectWidgetViews } from "../../api/types";
import type { WidgetBadgeSource } from "./widgetBadges";

export const PROJECT_WIDGET_VIEWS = {
  board: ["lanes", "done", "columns"], tests: ["runs"],
  schedules: ["schedules", "triggers", "event_sources"], rules: ["flow", "lint", "table", "last_runs"],
  agents: ["agents", "card_agents"], repos: ["list", "files"], wiki_index: ["browser"],
  memory: ["list", "graph"], db: ["types", "records"], pipelines: ["state", "runs"],
  calendar: ["month", "week", "agenda"], mail: ["messages"], events: ["events"],
} as const satisfies { [K in ProjectWidgetKind]: readonly ProjectWidgetViews[K][] };

export interface WidgetDefinition {
  kind: PageWidget["kind"];
  view?: ProjectWidgetViews[ProjectWidgetKind];
  part: string;
  host: "markdown" | "wiki" | "monitor" | "system" | "feature";
  badge?: WidgetBadgeSource;
}
const legacy = (kind: PageWidget["kind"], host: WidgetDefinition["host"]): WidgetDefinition => ({ kind, part: kind, host });
export const LEGACY_WIDGETS: readonly WidgetDefinition[] = [
  legacy("markdown", "markdown"), legacy("wiki", "wiki"),
  ...(["stat", "status", "chart", "text", "table"] as const).map(kind => legacy(kind, "monitor")),
  ...(["actions", "monitor_summary", "job_queue", "eta_accuracy"] as const).map(kind => legacy(kind, "system")),
];
export const WIDGET_REGISTRY: readonly WidgetDefinition[] = [
  ...LEGACY_WIDGETS,
  ...Object.entries(PROJECT_WIDGET_VIEWS).flatMap(([kind, views]) => views.map(view => ({
    kind: kind as ProjectWidgetKind, view, part: `${kind}:${view}`, host: "feature" as const,
    ...(kind === "board" || kind === "agents" ? { badge: kind } : {}),
  } satisfies WidgetDefinition))),
];

export function widgetDefinition(widget: Pick<PageWidget, "kind" | "options">): WidgetDefinition | undefined {
  const definitions = WIDGET_REGISTRY.filter(item => item.kind === widget.kind);
  return definitions.find(item => item.view === widget.options?.view) ??
    (widget.options?.view === undefined ? definitions[0] : undefined);
}

export function validFeatureOptions<K extends ProjectWidgetKind>(kind: K, options: unknown): boolean {
  if (options === undefined) return true;
  if (!options || typeof options !== "object" || Array.isArray(options)) return false;
  const value = options as Record<string, unknown>;
  return Object.keys(value).every(key => key === "search" || key === "view") &&
    (value.search === undefined || (typeof value.search === "string" && new TextEncoder().encode(value.search).length <= 256)) &&
    (value.view === undefined || (typeof value.view === "string" && (PROJECT_WIDGET_VIEWS[kind] as readonly string[]).includes(value.view)));
}

/** Typed per-kind builder. IDs belong to instances, never registry parts. */
export function featureWidget<K extends ProjectWidgetKind>(
  kind: K, view: ProjectWidgetViews[K], id: string, y: number, search?: string,
): PageWidget {
  if (!/^[A-Za-z0-9_-]{1,40}$/.test(id)) throw new Error("Invalid widget ID");
  if (!(PROJECT_WIDGET_VIEWS[kind] as readonly string[]).includes(view)) throw new Error("Invalid widget view");
  if (search !== undefined && new TextEncoder().encode(search).length > 256) throw new Error("Search exceeds 256 bytes");
  return { id, kind, x: 0, y, w: 12, h: 3, options: { view, ...(search === undefined ? {} : { search }) } };
}

export function widgetBadgeSources(widgets: readonly PageWidget[]): WidgetBadgeSource[] {
  return widgets.flatMap(widget => { const source = widgetDefinition(widget)?.badge; return source ? [source] : []; });
}
