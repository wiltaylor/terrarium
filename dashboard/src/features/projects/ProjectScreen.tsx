import { useInstance } from "../../instance";
// The shell of every project page. The first path segment selects a project
// tab, so deep links and browser history keep working without a route change.

import { useBeforeLeave, useParams, type RouteSectionProps } from "@solidjs/router";
import { useLocation } from "../../instance/navigation";
import { createEffect, on, onCleanup } from "solid-js";
import { PageHead } from "../../components/ui";
import { decodePath, decodePathParam, projectBase } from "../../lib/url";

import { ProjectTabs } from "./ProjectTabs";
import { blockedAgents, createProjectCounts, tabBadge } from "./tabCounts";
import { createLayoutEditor, LayoutEditorContext } from "../pages/layoutDraft";

export const LEAVE_DRAFT = "Discard your unsaved page layout changes?";

export default function ProjectScreen(props: RouteSectionProps) {
  const instance = useInstance();
  const { api, events, agents } = instance;
  const params = useParams<{ project: string }>();
  const location = useLocation();

  // Router params retain their URL encoding. Decode once for the title, then
  // encode the name once for every tab URL (including names with spaces).
  const project = () => decodePathParam(params.project);
  const base = () => projectBase(params.project);
  // The tab counts (TER-154): one bounded read per project and per change.
  const { counts, refresh } = createProjectCounts(api, events, project);
  createEffect(on(project, () => void refresh()));
  // One layout draft for all tabs of this project in this instance (TER-1073).
  const editor = createLayoutEditor(api, events, instance.key, project);
  const inProject = (to: string) => {
    const path = decodePath(instance.path(to.split(/[?#]/)[0]!)).replace(/\/$/, "");
    const target = decodePath(base());
    return path === target || path.startsWith(`${target}/`);
  };
  // Links, history and instance switches all leave through the router.
  useBeforeLeave(event => {
    if (!editor.dirty() || (typeof event.to === "string" && inProject(event.to))) return;
    if (window.confirm(LEAVE_DRAFT)) editor.cancel();
    else event.preventDefault();
  });
  const unload = (event: BeforeUnloadEvent) => {
    if (!editor.dirty()) return;
    event.preventDefault();
    event.returnValue = "";
  };
  window.addEventListener("beforeunload", unload);
  onCleanup(() => window.removeEventListener("beforeunload", unload));
  const badge = (path: string) => tabBadge(path, blockedAgents(agents.inProject(project())), counts());
  const active = (path: string) => {
    const current = decodePath(location.pathname).replace(/\/$/, "");
    const target = decodePath(`${base()}/${path}`);
    return current === target || current.startsWith(`${target}/`);
  };
  return (
    <LayoutEditorContext.Provider value={editor}>
      <PageHead title={project()} />
      <ProjectTabs base={base()} active={active} badge={badge} />
      {props.children}
    </LayoutEditorContext.Provider>
  );
}
