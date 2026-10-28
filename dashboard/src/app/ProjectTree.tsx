// The sidebar project tree: one node per project, open to show its pages.
// Keyboard follows the WAI-ARIA tree pattern with a roving tabindex.

import { A, useLocation } from "@solidjs/router";
import { createEffect, createMemo, createSignal, For, Show } from "solid-js";
import { agents, projects } from "../stores";
import { PROJECT_ITEMS } from "./nav";
import { createExpandedStore, filterProjects, FILTER_THRESHOLD, projectFromPath } from "./treeModel";

const expanded = createExpandedStore(
  (() => {
    try {
      return typeof localStorage === "undefined" ? null : localStorage;
    } catch {
      return null;
    }
  })(),
);

const projectKey = (name: string) => `p:${name}`;
const itemKey = (name: string, path: string) => `i:${name}/${path}`;
const href = (name: string, path: string) => `/p/${encodeURIComponent(name)}/${path}`;

export function ProjectTree() {
  const location = useLocation();
  const [query, setQuery] = createSignal("");
  const [focusKey, setFocusKey] = createSignal<string | null>(null);
  let tree: HTMLUListElement | undefined;

  const urlProject = () => projectFromPath(location.pathname);

  // The project you go to is open.
  createEffect(() => expanded.follow(location.pathname));
  // Forget open projects that are gone, once the list is in.
  createEffect(() => expanded.prune(projects.projects().map((p) => p.name)));

  const visible = () => filterProjects(projects.projects(), query());
  const showFilter = () => projects.projects().length > FILTER_THRESHOLD;

  // The rows in screen order, for the roving tabindex.
  const keys = createMemo(() =>
    visible().flatMap((p) => [projectKey(p.name), ...(expanded.isOpen(p.name) ? PROJECT_ITEMS.map((i) => itemKey(p.name, i.path)) : [])]),
  );
  // The one row that Tab reaches: the last focused row, else the URL project, else the first.
  const tabKey = () => {
    const k = keys();
    const f = focusKey();
    if (f && k.includes(f)) return f;
    const u = urlProject();
    if (u && k.includes(projectKey(u))) return projectKey(u);
    return k[0] ?? null;
  };
  const tabIndex = (key: string) => (tabKey() === key ? 0 : -1);

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

  const focusRow = (key: string) => {
    setFocusKey(key);
    const el = tree?.querySelector<HTMLElement>(`[data-key="${CSS.escape(key)}"]`);
    el?.focus();
  };

  const onKeyDown = (e: KeyboardEvent) => {
    const target = e.target as HTMLElement;
    const key = target.dataset.key;
    if (!key) return;
    const k = keys();
    const idx = k.indexOf(key);
    const project = target.dataset.project!;
    const isProject = key === projectKey(project);
    const move = (to: string | undefined) => {
      e.preventDefault();
      if (to) focusRow(to);
    };
    switch (e.key) {
      case "ArrowDown":
        return move(k[idx + 1]);
      case "ArrowUp":
        return move(k[idx - 1]);
      case "Home":
        return move(k[0]);
      case "End":
        return move(k[k.length - 1]);
      case "ArrowRight":
        if (!isProject) return;
        e.preventDefault();
        if (!expanded.isOpen(project)) expanded.open(project);
        else focusRow(k[idx + 1]!);
        return;
      case "ArrowLeft":
        e.preventDefault();
        if (isProject) expanded.close(project);
        else focusRow(projectKey(project));
        return;
      case "Enter":
      case " ":
        if (isProject) {
          e.preventDefault();
          activate(project);
        } else if (e.key === " ") {
          // Enter follows the link by itself; Space does not.
          e.preventDefault();
          target.click();
        }
        return;
    }
  };

  return (
    <div class="nav-section project-tree">
      <h4 id="project-tree-label">Projects</h4>
      <Show when={showFilter()}>
        <input
          type="search"
          class="tree-filter"
          placeholder="Filter projects"
          aria-label="Filter projects"
          value={query()}
          onInput={(e) => setQuery(e.currentTarget.value)}
        />
      </Show>
      <ul class="tree" role="tree" aria-labelledby="project-tree-label" ref={tree} onKeyDown={onKeyDown}>
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
      </ul>
    </div>
  );
}
