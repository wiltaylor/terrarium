// The shell of every project page. The first path segment selects a project
// tab, so deep links and browser history keep working without a route change.

import { useLocation, useParams, type RouteSectionProps } from "@solidjs/router";
import { PageHead } from "../../components/ui";
import { decodePath, decodePathParam, projectBase } from "../../lib/url";
import { ProjectTabs } from "./ProjectTabs";

export default function ProjectScreen(props: RouteSectionProps) {
  const params = useParams<{ project: string }>();
  const location = useLocation();

  // Router params retain their URL encoding. Decode once for the title, then
  // encode the name once for every tab URL (including names with spaces).
  const project = () => decodePathParam(params.project);
  const base = () => projectBase(params.project);
  const active = (path: string) => {
    const current = decodePath(location.pathname).replace(/\/$/, "");
    const target = decodePath(`${base()}/${path}`);
    return current === target || current.startsWith(`${target}/`);
  };
  return (
    <>
      <PageHead title={project()} />
      <ProjectTabs base={base()} active={active} />
      {props.children}
    </>
  );
}
