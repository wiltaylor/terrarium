// The dot before a project name (sidebar tree, project cards).

import type { ProjectHealthLevel } from "../lib/projectHealth";

/**
 * Red when an agent is in error or blocked, yellow when an agent works, green
 * when all agents rest (lib/projectHealth.ts). A project with no agents has a
 * hollow grey ring. The dot is not the only cue: the row label gives the counts.
 */
export function ProjectDot(props: { level: ProjectHealthLevel }) {
  return <span class={`project-dot health-${props.level}`} aria-hidden="true" />;
}
