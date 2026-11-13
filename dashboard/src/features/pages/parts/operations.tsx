// Project-page widget parts of Schedules, Rules, Pipelines and Calendar
// (TER-1075). Each part is one explicit-project panel from its feature
// folder. The final host (TER-1077) composes this map with the other
// part maps and dispatches on kind and view; this module does not render
// any page by itself.
//
// Rights: the dashboard has no caller-role snapshot, so mutation entry
// points need a resource-domain grant from the host. A missing grant is
// read-only. Layout `can_edit` is not a grant here. Calendar writes follow
// the server's `can_write` and per-event `can_edit`. The server stays
// authoritative for every write.

import type { Component } from "solid-js";
import { Show } from "solid-js";
import { Dynamic } from "solid-js/web";
import type { EventSource, PageWidget, ProjectWidgetViews, Schedule } from "../../../api/types";
import { CalendarPanel } from "../../calendar/CalendarPage";
import { PipelinesPanel } from "../../pipelines/PipelinesPage";
import { RulesPanel } from "../../rules/RulesPage";
import { SchedulesPanel } from "../../schedules/SchedulesPage";
import { widgetDefinition } from "../widgetRegistry";

export type OperationsKind = "schedules" | "rules" | "pipelines" | "calendar";

/** Resource-domain grants. Each field matches one server right. Absent means no right. */
export interface OperationsGrants {
  /** `manage_schedule` on the project: New schedule and New trigger. */
  createSchedule?: boolean;
  /** Edit, toggle and delete of one schedule or trigger (project or global scope). */
  manageSchedule?: (schedule: Schedule) => boolean;
  /** `admin_project`: New event source. */
  createEventSource?: boolean;
  /** Toggle, rotate and delete of one event source (project or global scope). */
  manageEventSource?: (source: EventSource) => boolean;
  /** `admin_project`: rule edits, enable and the rules migration. */
  manageRules?: boolean;
  /** `admin_project`: new, edit, delete and test of pipelines. */
  managePipelines?: boolean;
}

export interface OperationsPartProps {
  /** The project of the page. A widget never reads another project. */
  project: string;
  widget: PageWidget;
  grants?: OperationsGrants;
}

export type OperationsRenderers = {
  [K in OperationsKind]: { [V in ProjectWidgetViews[K]]: Component<OperationsPartProps> };
};

const search = (props: OperationsPartProps) => props.widget.options?.search || undefined;

const schedulesPart = (part: ProjectWidgetViews["schedules"]): Component<OperationsPartProps> => (props) =>
  <SchedulesPanel
    project={props.project}
    part={part}
    search={search(props)}
    canCreateSchedule={props.grants?.createSchedule === true}
    canCreateSource={props.grants?.createEventSource === true}
    canManageSchedule={(s) => props.grants?.manageSchedule?.(s) === true}
    canManageSource={(s) => props.grants?.manageEventSource?.(s) === true}
  />;

const rulesPart = (part: ProjectWidgetViews["rules"]): Component<OperationsPartProps> => (props) =>
  <RulesPanel project={props.project} part={part} search={search(props)} canManage={props.grants?.manageRules === true} />;

const pipelinesPart = (part: ProjectWidgetViews["pipelines"]): Component<OperationsPartProps> => (props) =>
  <PipelinesPanel project={props.project} part={part} search={search(props)} canManage={props.grants?.managePipelines === true} />;

// One calendar part. The saved view is the initial mode, month when absent.
// The user can still switch between all three modes in the widget; that
// choice is local to the instance and is not saved.
const calendarPart: Component<OperationsPartProps> = (props) =>
  <CalendarPanel project={props.project} view={(props.widget.options?.view as ProjectWidgetViews["calendar"] | undefined) ?? "month"} search={search(props)} />;

export const OPERATIONS_WIDGET_RENDERERS: OperationsRenderers = {
  schedules: { schedules: schedulesPart("schedules"), triggers: schedulesPart("triggers"), event_sources: schedulesPart("event_sources") },
  rules: { flow: rulesPart("flow"), lint: rulesPart("lint"), table: rulesPart("table"), last_runs: rulesPart("last_runs") },
  pipelines: { state: pipelinesPart("state"), runs: pipelinesPart("runs") },
  calendar: { month: calendarPart, week: calendarPart, agenda: calendarPart },
};

/** The renderer of a widget, or undefined for another kind or an unknown view (the host shows Not Found). */
export function operationsRenderer(widget: Pick<PageWidget, "kind" | "options">): Component<OperationsPartProps> | undefined {
  if (!Object.hasOwn(OPERATIONS_WIDGET_RENDERERS, widget.kind)) return undefined;
  const view = widgetDefinition(widget)?.view;
  if (view === undefined) return undefined;
  const parts = OPERATIONS_WIDGET_RENDERERS[widget.kind as OperationsKind] as Record<string, Component<OperationsPartProps>>;
  return Object.hasOwn(parts, view) ? parts[view] : undefined;
}

/** One widget of these four kinds, keyed on kind and view so a changed view mounts the new part. */
export function OperationsWidget(props: OperationsPartProps) {
  const renderer = () => operationsRenderer(props.widget);
  return <Show when={renderer()} keyed fallback={<p>Not Found</p>}>
    {(part) => <Dynamic component={part} project={props.project} widget={props.widget} grants={props.grants} />}
  </Show>;
}
