// One agent row of the sidebar tree, under the Agents node of a project.

import { A } from "@solidjs/router";
import type { Agent } from "../api/types";
import { lifecycleBadge } from "../lib/lifecycle";
import type { createRovingTree } from "./treeNav";

/**
 * One agent row: a link to the agent page, with a dot and a tint in the
 * colour of its status. A paused agent has a grey pause glyph, not a dot.
 * `current` marks the agent of the URL. The URL can
 * name the agent by ID or by `project/name`, so the href alone cannot.
 */
export function AgentRow(props: { agent: Agent; rowKey: string; nav: ReturnType<typeof createRovingTree>; project?: string; current?: boolean }) {
  const a = () => props.agent;
  // The status is in the label too, not in the colour only.
  const title = () => lifecycleBadge(a()).title;
  const label = () => `${a().project}/${a().name}, ${title()}`;
  return (
    <li role="none">
      <A
        href={`/agents/${encodeURIComponent(a().id)}`}
        class={`tree-agent st-${a().status}${a().paused ? " paused" : ""}${props.current ? " active" : ""}`}
        aria-current={props.current ? "page" : undefined}
        role="treeitem"
        aria-label={label()}
        title={`${a().project}/${a().name}\n${title()}`}
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
