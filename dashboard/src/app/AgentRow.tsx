// One agent row of the sidebar tree, under the Agents node of a project.

import { A } from "@solidjs/router";
import type { Agent } from "../api/types";
import { lifecycleBadge } from "../lib/lifecycle";
import { pins } from "../stores";
import type { createRovingTree } from "./treeNav";

/**
 * One agent row: a link to the agent page, with a dot and a tint in the
 * colour of its status. A paused agent has a grey pause glyph, not a dot.
 * `current` marks the agent of the URL. The URL can
 * name the agent by ID or by `project/name`, so the href alone cannot.
 * The pin button after the link pins the agent's conversation in the side
 * bar on the right. It is out of the tab order: in the tree, `p` does the same.
 */
export function AgentRow(props: { agent: Agent; rowKey: string; nav: ReturnType<typeof createRovingTree>; project?: string; current?: boolean }) {
  const a = () => props.agent;
  // The status is in the label too, not in the colour only.
  const title = () => lifecycleBadge(a()).title;
  const label = () => `${a().project}/${a().name}, ${title()}`;
  return (
    <li role="none" class="tree-agent-row">
      <A
        href={`/agents/${encodeURIComponent(a().id)}`}
        class={`tree-agent st-${a().status}${a().paused ? " paused" : ""}${props.current ? " active" : ""}`}
        aria-current={props.current ? "page" : undefined}
        role="treeitem"
        aria-label={label()}
        title={`${a().project}/${a().name}\n${title()}`}
        data-key={props.rowKey}
        data-project={props.project}
        data-agent={a().id}
        tabIndex={props.nav.tabIndex(props.rowKey)}
        onFocus={() => props.nav.setFocusKey(props.rowKey)}
      >
        <span class="status-dot" aria-hidden="true" />
        <span class="tree-name">{a().name}</span>
      </A>
      <button
        type="button"
        class="tree-pin"
        classList={{ pinned: pins.isPinned(a().id) }}
        tabIndex={-1}
        aria-pressed={pins.isPinned(a().id)}
        aria-label={`${pins.isPinned(a().id) ? "Unpin" : "Pin"} ${a().project}/${a().name}`}
        title={pins.isPinned(a().id) ? "Unpin the conversation (p)" : "Pin the conversation to the side bar on the right (p)"}
        onClick={() => void pins.toggle(a().id).catch(() => undefined)}
      >
        <PinGlyph />
      </button>
    </li>
  );
}

/** A small push-pin. */
export function PinGlyph() {
  return (
    <svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true" fill="currentColor">
      <path d="M10.5 1.5 14.5 5.5 13.4 6.6 12.6 5.8 9.9 8.5 10.2 11.6 9.1 12.7 6.6 10.2 2.8 14 2 13.2 5.8 9.4 3.3 6.9 4.4 5.8 7.5 6.1 10.2 3.4 9.4 2.6Z" />
    </svg>
  );
}
