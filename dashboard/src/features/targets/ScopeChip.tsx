import type { Target } from "../../api/types";
import { scopeLabel, targetScope } from "../../lib/target-scope";

/** The scope of a target: its project, or "global". */
export function ScopeChip(props: { target: Pick<Target, "scope" | "project"> }) {
  return (
    <span class={`chip ${targetScope(props.target) === "global" ? "info" : ""}`} title={targetScope(props.target) === "global" ? "A global target: any project" : `A target of project ${props.target.project}`}>
      {scopeLabel(props.target)}
    </span>
  );
}
