// The sidebar project tree: a Projects root that links the project cards,
// and under it one node per project, open to show its pages. Keyboard
// follows the WAI-ARIA tree pattern with a roving tabindex.

import { A, useLocation } from "@solidjs/router";
import { createEffect, createMemo, createSignal, For, Show } from "solid-js";
import { agents, projects } from "../stores";
import { PROJECT_ITEMS } from "./nav";
import { createExpandedStore, filterProjects, FILTER_THRESHOLD, projectFromPath, PROJECTS_ROOT } from "./treeModel";

const expanded = createExpandedStore(
  (() => {
    try {
      return typeof localStorage === "undefined" ? null : localStorage;
    } catch {
      return null;
    }
  })(),
);

const ROOT_KEY = "root";
const projectKey = (name: string) => `p:${name}`;
const itemKey = (name: string, path: string) => `i:${name}/${path}`;
const href = (name: string, path: string) => `/p/${encodeURIComponent(name)}/${path}`;

export function ProjectTree() {
  const location = useLocation();
  const [query, setQuery] = createSignal("");
  const [focusKey, setFocusKey] = createSignal<string | null>(null);
  let tree: HTMLUListElement | undefined;

  const urlProject = () => projectFromPath(location.pathname);
  const rootOpen = () => expanded.isOpen(PROJECTS_ROOT);

  // The project you go to is open, and so is the root.
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
  // The one row that Tab reaches: the last focused row, else the URL project, else the root.
  const tabKey = () => {
    const k = keys();
    const f = focusKey();
    if (f && k.includes(f)) return f;
    const u = urlProject();
    if (u && k.includes(projectKey(u))) return projectKey(u);
    return ROOT_KEY;
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
    const move = (to: string | undefined) => {
      e.preventDefault();
      if (to) focusRow(to);
    };
    if (key === ROOT_KEY) {
      switch (e.key) {
        case "ArrowDown":
          return move(k[idx + 1]);
        case "End":
          return move(k[k.length - 1]);
        case "Home":
        case "ArrowUp":
          return move(undefined);
        case "ArrowRight":
          e.preventDefault();
          if (!rootOpen()) expanded.open(PROJECTS_ROOT);
          else if (k[idx + 1]) focusRow(k[idx + 1]!);
          return;
        case "ArrowLeft":
          e.preventDefault();
          expanded.close(PROJECTS_ROOT);
          return;
        case " ":
          // Enter follows the link by itself; Space does not.
          e.preventDefault();
          target.click();
          return;
      }
      return;
    }
    const project = target.dataset.project!;
    const isProject = key === projectKey(project);
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
        if (isProject) {
          if (expanded.isOpen(project)) expanded.close(project);
          else focusRow(ROOT_KEY);
        } else focusRow(projectKey(project));
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
      <ul class="tree" role="tree" aria-label="Projects" ref={tree} onKeyDown={onKeyDown}>
        {/* The root is a link (WAI-ARIA navigation tree): its group follows it and aria-owns joins them. */}
        <li role="none">
          <A
            href="/projects"
            end
            activeClass="active"
            class="tree-row tree-root"
            role="treeitem"
            aria-expanded={rootOpen()}
            aria-owns="project-tree-root-group"
            data-key={ROOT_KEY}
            tabIndex={tabIndex(ROOT_KEY)}
            onFocus={() => setFocusKey(ROOT_KEY)}
            onClick={() => {
              expanded.open(PROJECTS_ROOT);
              setFocusKey(ROOT_KEY);
            }}
          >
            <span
              class={`tree-chevron${rootOpen() ? " open" : ""}`}
              aria-hidden="true"
              title={rootOpen() ? "Close" : "Open"}
              onClick={(e) => {
                // Toggle only: do not follow the link.
                e.preventDefault();
                e.stopPropagation();
                expanded.toggle(PROJECTS_ROOT);
                setFocusKey(ROOT_KEY);
              }}
            >
              ▸
            </span>
            <span class="tree-name">Projects</span>
          </A>
          <Show when={rootOpen() && showFilter()}>
            <input
              type="search"
              class="tree-filter"
              placeholder="Filter projects"
              aria-label="Filter projects"
              value={query()}
              onInput={(e) => setQuery(e.currentTarget.value)}
            />
          </Show>
          <Show when={rootOpen()}>
            <ul role="group" class="tree-group tree-root-group" id="project-tree-root-group">
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
          </Show>
        </li>
      </ul>
    </div>
  );
}
