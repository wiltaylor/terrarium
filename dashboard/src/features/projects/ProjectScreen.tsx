// The shell of every project page. The first path segment selects a project
// tab, so deep links and browser history keep working without a route change.

import { useLocation, useParams, type RouteSectionProps } from "@solidjs/router";
import { createEffect, on } from "solid-js";
import { PageHead } from "../../components/ui";
import { decodePath, decodePathParam, projectBase } from "../../lib/url";
import { agents, api, events } from "../../stores";
import { ProjectTabs } from "./ProjectTabs";
import { blockedAgents, createProjectCounts, tabBadge } from "./tabCounts";

export default function ProjectScreen(props: RouteSectionProps) {
  const params = useParams<{ project: string }>();
  const location = useLocation();

  // Router params retain their URL encoding. Decode once for the title, then
  // encode the name once for every tab URL (including names with spaces).
  const project = () => decodePathParam(params.project);
  const base = () => projectBase(params.project);
  // The tab counts (TER-154): one bounded read per project and per change.
  const { counts, refresh } = createProjectCounts(api, events, project);
  createEffect(on(project, () => void refresh()));
  const badge = (path: string) => tabBadge(path, blockedAgents(agents.inProject(project())), counts());
  const active = (path: string) => {
    const current = decodePath(location.pathname).replace(/\/$/, "");
    const target = decodePath(`${base()}/${path}`);
    return current === target || current.startsWith(`${target}/`);
  };
  return (
    <>
      <PageHead title={project()} />
      <ProjectTabs base={base()} active={active} badge={badge} />
      {props.children}
    </>
  );
}
