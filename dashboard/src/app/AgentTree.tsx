// The sidebar agent tree: an Agents root that links the agent list, and
// under it one row per agent with a dot in the colour of its status. Keyboard
// follows the WAI-ARIA tree pattern with a roving tabindex.

import { A, useLocation } from "@solidjs/router";
import { createEffect, createMemo, createSignal, For, Show } from "solid-js";
import { statusTitle } from "../components/StatusBadge";
import { agents, projects } from "../stores";
import { expanded, ROOT_KEY, TreeRoot } from "./TreeRoot";
import { AGENTS_ROOT, agentFromPath, filterAgents, FILTER_THRESHOLD, sortAgents } from "./treeModel";
import { createRovingTree } from "./treeNav";

const agentKey = (id: string) => `a:${id}`;

export function AgentTree() {
  const location = useLocation();
  const [query, setQuery] = createSignal("");

  const rootOpen = () => expanded.isOpen(AGENTS_ROOT);
  // The agent you go to is in view: the root opens. The Projects tree also
  // calls it; the second call does nothing.
  createEffect(() => expanded.follow(location.pathname));

  const all = createMemo(() => sortAgents(agents.state.agents));
  const visible = () => filterAgents(all(), query());
  const showFilter = () => all().length > FILTER_THRESHOLD;
  // The URL takes an ID or `project/name` (agents.byId).
  const urlAgent = () => {
    const id = agentFromPath(location.pathname);
    return id ? (agents.byId(id)?.id ?? null) : null;
  };

  const keys = createMemo(() => [ROOT_KEY, ...(rootOpen() ? visible().map((a) => agentKey(a.id)) : [])]);
  // Tab reaches the last focused row, else the URL agent, else the root.
  const nav = createRovingTree({
    keys,
    preferred: () => {
      const id = urlAgent();
      return id ? agentKey(id) : null;
    },
  });

  // The same count as before the tree: blocked or in error in the selected project.
  const blocked = () => agents.inProject(projects.selected()).filter((a) => a.status === "blocked" || a.status === "error").length;

  const onKeyDown = (e: KeyboardEvent) =>
    nav.onKeyDown(e, (target) => {
      const key = target.dataset.key;
      if (!key) return null;
      if (key === ROOT_KEY) return { key, open: rootOpen(), onOpen: () => expanded.open(AGENTS_ROOT), onClose: () => expanded.close(AGENTS_ROOT) };
      return { key, parent: ROOT_KEY };
    });

  return (
    <ul class="tree agent-tree" role="tree" aria-label="Agents" ref={nav.ref} onKeyDown={onKeyDown}>
      <TreeRoot
        label="Agents"
        href="/agents"
        groupId="agent-tree-root-group"
        open={rootOpen()}
        onOpen={() => expanded.open(AGENTS_ROOT)}
        onToggle={() => expanded.toggle(AGENTS_ROOT)}
        nav={nav}
        badge={
          <Show when={blocked() > 0}>
            <span class="nav-count" title="Blocked or in error in this project">
              {blocked()}
            </span>
          </Show>
        }
        showFilter={showFilter()}
        query={query()}
        onQuery={setQuery}
      >
        <For each={visible()} fallback={<li class="tree-empty muted small">{query() ? "No agent matches." : "No agents."}</li>}>
          {(a) => {
            // The status is in the label too, not in the colour only.
            const label = () => `${a.project}/${a.name}, ${statusTitle(a.status, a.waiting_on)}`;
            return (
              <li role="none">
                <A
                  href={`/agents/${encodeURIComponent(a.id)}`}
                  activeClass="active"
                  class={`tree-agent st-${a.status}`}
                  role="treeitem"
                  aria-label={label()}
                  title={`${a.project}/${a.name}\n${statusTitle(a.status, a.waiting_on)}`}
                  data-key={agentKey(a.id)}
                  tabIndex={nav.tabIndex(agentKey(a.id))}
                  onFocus={() => nav.setFocusKey(agentKey(a.id))}
                >
                  <span class="status-dot" aria-hidden="true" />
                  <span class="tree-name">{a.name}</span>
                  <span class="tree-sub">{a.project}</span>
                </A>
              </li>
            );
          }}
        </For>
      </TreeRoot>
    </ul>
  );
}
