// One agent row of the sidebar tree, under the Agents node of a project.

import { A } from "@solidjs/router";
import type { Agent } from "../api/types";
import { statusTitle } from "../components/StatusBadge";
import type { createRovingTree } from "./treeNav";

/**
 * One agent row: a link to the agent page, with a dot and a tint in the
 * colour of its status. `current` marks the agent of the URL. The URL can
 * name the agent by ID or by `project/name`, so the href alone cannot.
 */
export function AgentRow(props: { agent: Agent; rowKey: string; nav: ReturnType<typeof createRovingTree>; project?: string; current?: boolean }) {
  const a = () => props.agent;
  // The status is in the label too, not in the colour only.
  const label = () => `${a().project}/${a().name}, ${statusTitle(a().status, a().waiting_on)}`;
  return (
    <li role="none">
      <A
        href={`/agents/${encodeURIComponent(a().id)}`}
        class={`tree-agent st-${a().status}${props.current ? " active" : ""}`}
        aria-current={props.current ? "page" : undefined}
        role="treeitem"
        aria-label={label()}
        title={`${a().project}/${a().name}\n${statusTitle(a().status, a().waiting_on)}`}
        data-key={props.rowKey}
        data-project={props.project}
        tabIndex={props.nav.tabIndex(props.rowKey)}
        onFocus={() => props.nav.setFocusKey(props.rowKey)}
      >
        <span class="status-dot" aria-hidden="true" />
        <span class="tree-name">{a().name}</span>
      </A>
    </li>
  );
}
