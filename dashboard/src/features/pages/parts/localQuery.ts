// Widget-local query state for the Board, Tests and Agents widgets (TER-1074).
// A route page keeps its filters in the URL. A widget instance keeps them in
// memory, so two widgets on one page never write the same URL keys.

import type { PageWidget, ProjectWidgetKind, ProjectWidgetViews } from "../../../api/types";
import { PROJECT_WIDGET_VIEWS } from "../widgetRegistry";

/** Merge a query patch: an undefined or empty value removes the key. */
export function patchQuery<Q extends Record<string, string | undefined>>(query: Q, patch: Partial<Q>): Q {
  const next: Record<string, string | undefined> = { ...query };
  for (const [key, value] of Object.entries(patch)) {
    if (value === undefined || value === "") delete next[key];
    else next[key] = value;
  }
  return next as Q;
}

/** The saved view of a widget, or the first view of its kind when absent. Undefined for an unknown view. */
export function widgetView<K extends ProjectWidgetKind>(kind: K, options: PageWidget["options"]): ProjectWidgetViews[K] | undefined {
  const views: readonly string[] = PROJECT_WIDGET_VIEWS[kind];
  const view = options?.view ?? views[0];
  return view !== undefined && views.includes(view) ? (view as ProjectWidgetViews[K]) : undefined;
}

/** The saved search of a widget, trimmed. Empty means no search. */
export function widgetSearch(options: PageWidget["options"]): string | undefined {
  return options?.search?.trim() || undefined;
}
