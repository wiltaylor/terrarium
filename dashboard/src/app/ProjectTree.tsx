// The sidebar project tree: a Projects root that links the project cards,
// and under it one node per project, open to show its pages. The Agents page
// of a project is a node too: open, it shows the agents of that project.
// Keyboard follows the WAI-ARIA tree pattern with a roving tabindex.

import { A, useLocation } from "@solidjs/router";
import { createEffect, createMemo, createSignal, For, Show } from "solid-js";
import type { Agent } from "../api/types";
import { agents, projects } from "../stores";
import { AgentRow } from "./AgentRow";
import { PROJECT_ITEMS } from "./nav";
import { expanded, ROOT_KEY, TreeRoot } from "./TreeRoot";
import {
  filterAgents,
  filterProjects,
  FILTER_THRESHOLD,
  itemKey,
  agentFromPath,
  projectAgentKey,
  projectAgentsNode,
  projectFromPath,
  projectKey,
  PROJECTS_ROOT,
  projectTreeKeys,
  sortAgents,
} from "./treeModel";
import { createRovingTree } from "./treeNav";

const href = (name: string, path: string) => `/p/${encodeURIComponent(name)}/${path}`;

export function ProjectTree() {
  const location = useLocation();
  const [query, setQuery] = createSignal("");

  const urlProject = () => projectFromPath(location.pathname);
  // The agent of an agent page, by ID or `project/name` (agents.byId).
  const urlAgent = () => {
    const id = agentFromPath(location.pathname);
    return id ? (agents.byId(id)?.id ?? null) : null;
  };
  const rootOpen = () => expanded.isOpen(PROJECTS_ROOT);

  // The project you go to is open, and so is the root. The Agents node of a
  // project opens on its agent list. An agent page opens the root, the
  // project of the agent, and its Agents node. The agent page takes an ID or
  // `project/name` (agents.byId).
  createEffect(() => expanded.follow(location.pathname, (id) => agents.byId(id)?.project));
  // Forget open projects that are gone, once the list is in.
  createEffect(() => expanded.prune(projects.projects().map((p) => p.name)));

  const visible = () => filterProjects(projects.projects(), query());
  const showFilter = () => projects.projects().length > FILTER_THRESHOLD;

  // The filter of each project Agents node, by project.
  const [agentQueries, setAgentQueries] = createSignal<Record<string, string>>({});
  const projectAgents = (project: string) => sortAgents(agents.inProject(project));
  const visibleAgents = (project: string) => filterAgents(projectAgents(project), agentQueries()[project] ?? "");

  // The rows in screen order, for the roving tabindex.
  const keys = createMemo(() =>
    projectTreeKeys({
      rootKey: ROOT_KEY,
      rootOpen: rootOpen(),
      projects: visible().map((p) => p.name),
      items: PROJECT_ITEMS.map((i) => i.path),
      isOpen: expanded.isOpen,
      agentsOf: (p) => visibleAgents(p).map((a) => a.id),
    }),
  );
  // Tab reaches the last focused row, else the URL agent, else the URL project, else the root.
  const nav = createRovingTree({
    keys,
    preferred: () => {
      const a = urlAgent();
      if (a) return projectAgentKey(a);
      const u = urlProject();
      return u ? projectKey(u) : null;
    },
  });
  const tabIndex = nav.tabIndex;
  const setFocusKey = nav.setFocusKey;

  const blocked = (name: string) => agents.inProject(name).filter((a) => a.status === "blocked" || a.status === "error").length;

  /**
   * The name toggles the node. Off a project page it also selects the project,
   * so global pages (Agents, the Events tab of Logs) scope to it. On a project page the URL
   * is the selection, so the name only toggles.
   */
  const activate = (name: string) => {
    expanded.toggle(name);
    setFocusKey(projectKey(name));
    if (!urlProject() && projects.selected() !== name) projects.select(name);
  };

  const onKeyDown = (e: KeyboardEvent) =>
    nav.onKeyDown(e, (target) => {
      const key = target.dataset.key;
      if (!key) return null;
      if (key === ROOT_KEY)
        return {
          key,
          open: rootOpen(),
          onOpen: () => expanded.open(PROJECTS_ROOT),
          onClose: () => expanded.close(PROJECTS_ROOT),
        };
      const project = target.dataset.project!;
      const agentsItem = itemKey(project, "agents");
      if (key === agentsItem) {
        const node = projectAgentsNode(project);
        return { key, parent: projectKey(project), open: expanded.isOpen(node), onOpen: () => expanded.open(node), onClose: () => expanded.close(node) };
      }
      if (key.startsWith("pa:")) return { key, parent: agentsItem };
      if (key !== projectKey(project)) return { key, parent: projectKey(project) };
      return {
        key,
        parent: ROOT_KEY,
        open: expanded.isOpen(project),
        button: true,
        onOpen: () => expanded.open(project),
        onClose: () => expanded.close(project),
        onActivate: () => activate(project),
      };
    });

  return (
    <div class="nav-section project-tree">
      <ul class="tree" role="tree" aria-label="Projects" ref={nav.ref} onKeyDown={onKeyDown}>
        <TreeRoot
          label="Projects"
          href="/projects"
          groupId="project-tree-root-group"
          open={rootOpen()}
          onOpen={() => expanded.open(PROJECTS_ROOT)}
          onToggle={() => expanded.toggle(PROJECTS_ROOT)}
          nav={nav}
          showFilter={showFilter()}
          query={query()}
          onQuery={setQuery}
        >
          <For each={visible()} fallback={<li class="tree-empty muted small">{query() ? "No project matches." : "No projects."}</li>}>
            {(p) => {
              const open = () => expanded.isOpen(p.name);
              const current = () => urlProject() === p.name;
              return (
                <li
                  role="treeitem"
                  aria-expanded={open()}
                  aria-selected={current()}
                  aria-label={p.name}
                  data-key={projectKey(p.name)}
                  data-project={p.name}
                  tabIndex={tabIndex(projectKey(p.name))}
                  onFocus={(e) => {
                    if (e.target === e.currentTarget) setFocusKey(projectKey(p.name));
                  }}
                >
                  <div class={`tree-row${current() ? " current" : ""}`} onClick={() => activate(p.name)}>
                    <span
                      class={`tree-chevron${open() ? " open" : ""}`}
                      aria-hidden="true"
                      onClick={(e) => {
                        e.stopPropagation();
                        expanded.toggle(p.name);
                        setFocusKey(projectKey(p.name));
                      }}
                    >
                      ▸
                    </span>
                    <span class="tree-name" title={p.name}>
                      {p.name}
                    </span>
                    <Show when={blocked(p.name) > 0}>
                      <span class="nav-count warn" title="Agents blocked or in error in this project">
                        {blocked(p.name)}
                      </span>
                    </Show>
                  </div>
                  <Show when={open()}>
                    <ul role="group" class="tree-group">
                      <For each={PROJECT_ITEMS}>
                        {(item) => (
                          <Show
                            when={item.path !== "agents"}
                            fallback={
                              <ProjectAgentsNode
                                project={p.name}
                                label={item.label}
                                nav={nav}
                                total={projectAgents(p.name).length}
                                blocked={blocked(p.name)}
                                agents={visibleAgents(p.name)}
                                currentAgent={urlAgent()}
                                query={agentQueries()[p.name] ?? ""}
                                onQuery={(q) => setAgentQueries((all) => ({ ...all, [p.name]: q }))}
                              />
                            }
                          >
                            <li role="none">
                              <A
                                href={href(p.name, item.path)}
                                activeClass="active"
                                role="treeitem"
                                data-key={itemKey(p.name, item.path)}
                                data-project={p.name}
                                tabIndex={tabIndex(itemKey(p.name, item.path))}
                                onFocus={() => setFocusKey(itemKey(p.name, item.path))}
                              >
                                <span>{item.label}</span>
                              </A>
                            </li>
                          </Show>
                        )}
                      </For>
                    </ul>
                  </Show>
                </li>
              );
            }}
          </For>
        </TreeRoot>
      </ul>
    </div>
  );
}

/** The Agents page of a project as a node: the label goes to the page and opens the node; the chevron only toggles. */
function ProjectAgentsNode(props: {
  project: string;
  label: string;
  nav: ReturnType<typeof createRovingTree>;
  /** The agents of the project before the filter. */
  total: number;
  /** The agents of the project that are blocked or in error. */
  blocked: number;
  /** The agents to show: sorted and filtered. */
  agents: Agent[];
  /** The ID of the agent of the URL, or null. */
  currentAgent: string | null;
  query: string;
  onQuery: (q: string) => void;
}) {
  const node = () => projectAgentsNode(props.project);
  return (
    <TreeRoot
      label={props.label}
      href={href(props.project, "agents")}
      rowKey={itemKey(props.project, "agents")}
      rowClass="tree-node"
      project={props.project}
      groupId={`project-agents-${props.project}`}
      open={expanded.isOpen(node())}
      onOpen={() => expanded.open(node())}
      onToggle={() => expanded.toggle(node())}
      nav={props.nav}
      badge={
        <Show when={props.blocked > 0}>
          <span class="nav-count warn" title="Agents blocked or in error in this project">
            {props.blocked}
          </span>
        </Show>
      }
      showFilter={props.total > FILTER_THRESHOLD}
      query={props.query}
      onQuery={props.onQuery}
    >
      <For each={props.agents} fallback={<li class="tree-empty muted small">{props.query ? "No agent matches." : "No agents."}</li>}>
        {(a) => <AgentRow agent={a} rowKey={projectAgentKey(a.id)} nav={props.nav} project={props.project} current={a.id === props.currentAgent} />}
      </For>
    </TreeRoot>
  );
}
