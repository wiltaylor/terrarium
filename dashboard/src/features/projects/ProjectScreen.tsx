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

/**
 * The draft stays only inside the same project of the same instance. Compare
 * full browser paths: `/p/demo` of the home and `/remote/a/p/demo` are two
 * instances, even with the same project name. A history step (a number) leaves.
 */
export function leavesProject(href: (path: string) => string, base: string, to: string | number): boolean {
  if (typeof to !== "string") return true;
  const path = decodePath(to.split(/[?#]/)[0]!).replace(/\/$/, "");
  const target = decodePath(href(base));
  return path !== target && !path.startsWith(`${target}/`);
}

type Url = { pathname: string; search: string; hash: string };
const full = (url: Url) => url.pathname + url.search + url.hash;
/**
 * The browser Router asks beforeLeave on Back and Forward with the delta only,
 * after `popstate`: the address bar already shows the target, and the router
 * location (`from`) is still the old page. A memory router leaves the address
 * bar alone, so the two agree and the step stays unknown (it leaves).
 */
export function historyTarget(to: string | number, from: Url, address: Url = window.location): string | number {
  return typeof to !== "number" || full(address) === full(from) ? to : full(address);
}

export const LEAVE_DRAFT = "Unsaved page layout changes are lost if you leave this project. Continue?";

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
  // Links, history and instance switches (RemoteScope is a route) all leave
  // through the router, so one guard covers them. It never ends the draft:
  // Back inside the project keeps it without a prompt, and leaving the
  // project unmounts this screen or changes the scope, which ends it.
  useBeforeLeave(event => {
    if (!editor.dirty() || !leavesProject(instance.href, base(), historyTarget(event.to, event.from))) return;
    if (!window.confirm(LEAVE_DRAFT)) event.preventDefault();
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
