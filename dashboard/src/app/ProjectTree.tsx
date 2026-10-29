// The sidebar project tree: a Projects root that links the project cards,
// and under it one node per project, open to show its pages. Keyboard
// follows the WAI-ARIA tree pattern with a roving tabindex.

import { A, useLocation } from "@solidjs/router";
import { createEffect, createMemo, createSignal, For, Show } from "solid-js";
import { agents, projects } from "../stores";
import { PROJECT_ITEMS } from "./nav";
import { expanded, ROOT_KEY, TreeRoot } from "./TreeRoot";
import { filterProjects, FILTER_THRESHOLD, projectFromPath, PROJECTS_ROOT } from "./treeModel";
import { createRovingTree } from "./treeNav";

const projectKey = (name: string) => `p:${name}`;
const itemKey = (name: string, path: string) => `i:${name}/${path}`;
const href = (name: string, path: string) => `/p/${encodeURIComponent(name)}/${path}`;

export function ProjectTree() {
  const location = useLocation();
  const [query, setQuery] = createSignal("");

  const urlProject = () => projectFromPath(location.pathname);
  const rootOpen = () => expanded.isOpen(PROJECTS_ROOT);

  // The project you go to is open, and so is the root. The Agents tree also
  // calls it; the second call does nothing.
  createEffect(() => expanded.follow(location.pathname));
  // Forget open projects that are gone, once the list is in.
  createEffect(() => expanded.prune(projects.projects().map((p) => p.name)));

  const visible = () => filterProjects(projects.projects(), query());
  const showFilter = () => projects.projects().length > FILTER_THRESHOLD;

  // The rows in screen order, for the roving tabindex.
  const keys = createMemo(() => [
    ROOT_KEY,
    ...(rootOpen()
      ? visible().flatMap((p) => [projectKey(p.name), ...(expanded.isOpen(p.name) ? PROJECT_ITEMS.map((i) => itemKey(p.name, i.path)) : [])])
      : []),
  ]);
  // Tab reaches the last focused row, else the URL project, else the root.
  const nav = createRovingTree({
    keys,
    preferred: () => {
      const u = urlProject();
      return u ? projectKey(u) : null;
    },
  });
  const tabIndex = nav.tabIndex;
  const setFocusKey = nav.setFocusKey;

  const blocked = (name: string) => agents.inProject(name).filter((a) => a.status === "blocked" || a.status === "error").length;

  /**
   * The name toggles the node. Off a project page it also selects the project,
   * so global pages (Agents, Event log) scope to it. On a project page the URL
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
