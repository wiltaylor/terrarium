// The sidebar project tree: a Projects root that links the project cards,
// and under it one node per project, open to show its pages. Agents and
// Repos are nodes, not pages: open, they show the agents and the repos of
// that project, and a "+" at the right of each adds one. Keyboard follows
// the WAI-ARIA tree pattern with a roving tabindex. A health dot before
// each project name gives the state of its agents, and a cog at the right
// of each project row opens the project settings screen.
//
// Project folders (docs/api.md §6.12, TER-48): for the operator and a
// sysadmin, folders group the projects, nested. A "+" on the Projects row
// adds a folder, the "⋯" of a row has Move to folder (and for a folder New
// subfolder, Rename, and Delete), and a project or folder row can be dragged
// onto a folder row or onto the Projects row (the top level). With a filter
// typed, the tree shows the plain list of matching projects.

import { A, useLocation, useNavigate } from "@solidjs/router";
import { createEffect, createMemo, createSignal, For, lazy, Match, onMount, Show, Switch, untrack } from "solid-js";
import type { Agent, Project, ProjectFolder, Repo } from "../api/types";
import { ProjectDot } from "../components/ProjectDot";
import { ErrorBanner } from "../components/ui";
import { DeleteFolderDialog, FolderNameDialog, MoveToFolderDialog, RowMenu } from "../features/projectFolders/FolderDialogs";
import {
  ancestors,
  buildTree,
  canMoveTo,
  folderKey,
  folderNode,
  folderPath,
  folderTreeKeys,
  parentFolders,
  projectAncestors,
  projectCount,
  type Moving,
  type TreeEntry,
} from "../features/projectFolders/model";
import { AddRepoDialog } from "../features/repos/AddRepoForm";
import { projectHealth, projectHealthLabel } from "../lib/projectHealth";
import { agents, pins, projectFolders, projects, repos } from "../stores";
import { AgentRow } from "./AgentRow";
import { PROJECT_ITEMS } from "./nav";
import { expanded, ROOT_KEY, TreeNode, TreeRoot, type DropTarget } from "./TreeRoot";
import {
  filterAgents,
  filterProjects,
  filterRepos,
  FILTER_THRESHOLD,
  itemKey,
  agentFromPath,
  projectAgentKey,
  projectAgentsNode,
  projectFromPath,
  projectKey,
  projectRepoKey,
  projectReposNode,
  PROJECTS_ROOT,
  projectTreeKeys,
  repoFromPath,
  sortAgents,
} from "./treeModel";
import { createRovingTree } from "./treeNav";
import { Bookmarks, BOOKMARKS_KEY, bookmarkKey } from "./Bookmarks";
import { bookmarks } from "../stores";

// The template chooser loads with the dialog, not with the sidebar.
const NewAgentDialog = lazy(() => import("../features/agents/NewAgentDialog").then((m) => ({ default: m.NewAgentDialog })));

const href = (name: string, path: string) => `/p/${encodeURIComponent(name)}/${path}`;
const repoHref = (project: string, repo: string) => href(project, `repos/${encodeURIComponent(repo)}`);

/** The add dialog that a "+" of the tree opened. */
type Adding = { kind: "agent" | "repo"; project: string };

/** The open folder dialog. */
type FolderDialog =
  | { kind: "new"; parent: string | null }
  | { kind: "rename"; folder: ProjectFolder }
  | { kind: "delete"; folder: ProjectFolder; projects: number }
  | { kind: "move"; what: Moving; label: string };

/** The drag data type of a project or folder row. */
const DRAG_TYPE = "application/x-terrarium-tree";

/** The `data-key` of each entry: a folder row key, or a project row key. */
const entryKey = (e: TreeEntry) => (e.kind === "folder" ? folderKey(e.folder.id) : projectKey(e.name));

export function ProjectTree() {
  const location = useLocation();
  const navigate = useNavigate();
  const [adding, setAdding] = createSignal<Adding | null>(null);
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
  // Forget open projects and folders that are gone, once the lists are in.
  createEffect(() =>
    expanded.prune(
      projects.projects().map((p) => p.name),
      projectFolders.available() ? projectFolders.folders().map((f) => f.id) : null,
    ),
  );

  const visible = () => filterProjects(projects.projects(), query());
  const showFilter = () => projects.projects().length > FILTER_THRESHOLD;

  // ----- project folders -----
  onMount(() => void projectFolders.refresh());
  const folderUi = () => projectFolders.available();
  /** The folder tree: folders on, and no filter typed. */
  const useTree = () => folderUi() && !query().trim();
  const tree = createMemo(() =>
    buildTree(
      projectFolders.folders(),
      projectFolders.placements(),
      visible().map((p) => p.name),
    ),
  );
  const entries = createMemo(() => {
    const m = new Map<string, TreeEntry>();
    const walk = (list: TreeEntry[]) => {
      for (const e of list) {
        m.set(entryKey(e), e);
        if (e.kind === "folder") walk(e.children);
      }
    };
    walk(tree());
    return m;
  });
  const parents = createMemo(() => parentFolders(tree()));
  const projectByName = createMemo(() => new Map(projects.projects().map((p) => [p.name, p])));
  const [folderDialog, setFolderDialog] = createSignal<FolderDialog | null>(null);

  // The folders above the project of a new URL open, so the project is in view.
  let shownProject: string | null = null;
  createEffect(() => {
    const p = urlProject();
    if (!p || !folderUi()) return;
    const f = projectFolders.folders();
    const pl = projectFolders.placements();
    if (p === shownProject) return;
    shownProject = p;
    untrack(() => {
      for (const id of projectAncestors(f, pl, p)) expanded.open(folderNode(id));
    });
  });

  // Drag and drop: the dragged row is in a signal, since a dragover cannot read the drag data.
  const [dragging, setDragging] = createSignal<Moving | null>(null);
  // The folder ID under the drag, null for the Projects row, undefined for none.
  const [dropAt, setDropAt] = createSignal<string | null | undefined>(undefined);
  const canDrop = (what: Moving | null, target: string | null) =>
    !!what && canMoveTo(projectFolders.folders(), projectFolders.placements(), what, target);
  const dragStart = (what: Moving, label: string) => (e: DragEvent) => {
    e.stopPropagation();
    setDragging(what);
    if (e.dataTransfer) {
      e.dataTransfer.setData(DRAG_TYPE, JSON.stringify(what));
      e.dataTransfer.setData("text/plain", label);
      e.dataTransfer.effectAllowed = "move";
    }
  };
  const dragEnd = () => {
    setDragging(null);
    setDropAt(undefined);
  };
  const carried = (e: DragEvent): Moving | null => {
    const d = dragging();
    if (d) return d;
    try {
      const raw = e.dataTransfer?.getData(DRAG_TYPE);
      return raw ? (JSON.parse(raw) as Moving) : null;
    } catch {
      return null;
    }
  };
  const dropTarget = (target: () => string | null): DropTarget => ({
    get over() {
      return dropAt() === target();
    },
    onDragOver: (e) => {
      if (!canDrop(dragging(), target())) return;
      e.preventDefault();
      e.stopPropagation();
      if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
      setDropAt(target());
    },
    onDragLeave: () => {
      if (dropAt() === target()) setDropAt(undefined);
    },
    onDrop: (e) => {
      const what = carried(e);
      const to = target();
      dragEnd();
      if (!what || !canDrop(what, to)) return;
      e.preventDefault();
      e.stopPropagation();
      moveTo(what, to);
    },
  });
  /** Move a project or folder; the target folder opens, so the row stays in view. */
  const moveTo = (what: Moving, to: string | null) => {
    if (to) for (const id of [...ancestors(projectFolders.folders(), to), to]) expanded.open(folderNode(id));
    return projectFolders.move(what, to);
  };
  const folderLabel = (id: string) => `the folder ${projectFolders.folders().find((f) => f.id === id)?.name ?? ""}`;
  const moveProject = (name: string) => setFolderDialog({ kind: "move", what: { kind: "project", name }, label: name });
  const folderItems = (f: ProjectFolder) => [
    { label: "New subfolder", key: "+", run: () => setFolderDialog({ kind: "new", parent: f.id }) },
    { label: "Rename", key: "F2", run: () => setFolderDialog({ kind: "rename", folder: f }) },
    { label: "Move to folder…", key: "m", run: () => setFolderDialog({ kind: "move", what: { kind: "folder", id: f.id }, label: folderLabel(f.id) }) },
    { label: "Delete", key: "Delete", run: () => askDelete(f) },
  ];
  const rootDrop = dropTarget(() => null);
  const askDelete = (f: ProjectFolder) => {
    const e = entries().get(folderKey(f.id));
    setFolderDialog({ kind: "delete", folder: f, projects: e ? projectCount(e) : 0 });
  };

  // The filter of each project Agents node, by project.
  const [agentQueries, setAgentQueries] = createSignal<Record<string, string>>({});
  const projectAgents = (project: string) => sortAgents(agents.inProject(project));
  const visibleAgents = (project: string) => filterAgents(projectAgents(project), agentQueries()[project] ?? "");
  // The filter of each project Repos node, by project.
  const [repoQueries, setRepoQueries] = createSignal<Record<string, string>>({});
  const visibleRepos = (project: string) => filterRepos(repos.inProject(project), repoQueries()[project] ?? "");
  // The project and the repo of a repo page.
  const urlRepo = () => {
    const p = urlProject();
    const r = repoFromPath(location.pathname);
    return p && r ? projectRepoKey(p, r) : null;
  };

  // The rows in screen order, for the roving tabindex.
  const treeKeys = (rootOpen: boolean, names: string[]) =>
    projectTreeKeys({
      rootKey: ROOT_KEY,
      rootOpen,
      projects: names,
      items: PROJECT_ITEMS.map((i) => i.path),
      isOpen: expanded.isOpen,
      agentsOf: (p) => visibleAgents(p).map((a) => a.id),
      reposOf: (p) => visibleRepos(p).map((r) => r.name),
    });
  const keys = createMemo(() => {
    const base = !rootOpen() || !useTree() ? treeKeys(rootOpen(), visible().map((p) => p.name)) : [ROOT_KEY, ...folderTreeKeys(tree(), expanded.isOpen, (p) => treeKeys(true, [p]).slice(1))];
    return rootOpen() ? [...base, BOOKMARKS_KEY, ...(expanded.isOpen(BOOKMARKS_KEY) ? bookmarks.items().map((b) => bookmarkKey(b.href)) : [])] : base;
  });
  // Tab reaches the last focused row, else the URL agent, else the URL project, else the root.
  const nav = createRovingTree({
    keys,
    preferred: () => {
      const a = urlAgent();
      if (a) return projectAgentKey(a);
      const r = urlRepo();
      if (r) return r;
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
      if (key === BOOKMARKS_KEY) return {
        key, parent: ROOT_KEY, button: true, open: expanded.isOpen(BOOKMARKS_KEY),
        onOpen: () => expanded.open(BOOKMARKS_KEY), onClose: () => expanded.close(BOOKMARKS_KEY), onActivate: () => expanded.toggle(BOOKMARKS_KEY),
      };
      if (key.startsWith("bookmark:")) return { key, parent: BOOKMARKS_KEY };
      if (key === ROOT_KEY)
        return {
          key,
          open: rootOpen(),
          add: folderUi(),
          onOpen: () => expanded.open(PROJECTS_ROOT),
          onClose: () => expanded.close(PROJECTS_ROOT),
          onAdd: () => setFolderDialog({ kind: "new", parent: null }),
        };
      if (key.startsWith("f:")) {
        const f = projectFolders.folders().find((x) => x.id === target.dataset.folder);
        if (!f) return null;
        const node = folderNode(f.id);
        const [add, rename, move, remove] = folderItems(f).map((i) => i.run);
        return {
          key,
          parent: parents().folders.get(f.id) ?? ROOT_KEY,
          open: expanded.isOpen(node),
          button: true,
          add: true,
          move: true,
          edit: true,
          onOpen: () => expanded.open(node),
          onClose: () => expanded.close(node),
          onActivate: () => expanded.toggle(node),
          onAdd: add,
          onRename: rename,
          onMove: move,
          onRemove: remove,
        };
      }
      const project = target.dataset.project!;
      const agentsItem = itemKey(project, "agents");
      const reposItem = itemKey(project, "repos");
      // The Agents and Repos nodes are not links: Enter and Space toggle them, and `+` adds one.
      const node = key === agentsItem ? projectAgentsNode(project) : key === reposItem ? projectReposNode(project) : null;
      if (node)
        return {
          key,
          parent: projectKey(project),
          open: expanded.isOpen(node),
          button: true,
          add: true,
          onOpen: () => expanded.open(node),
          onClose: () => expanded.close(node),
          onActivate: () => expanded.toggle(node),
          onAdd: () => setAdding({ kind: key === agentsItem ? "agent" : "repo", project }),
        };
      // An agent row: `p` pins or unpins its conversation (the side bar on the right).
      if (key.startsWith("pa:")) {
        const id = target.dataset.agent;
        return { key, parent: agentsItem, pin: !!id, onPin: () => id && void pins.toggle(id).catch(() => undefined) };
      }
      if (key.startsWith("pr:")) return { key, parent: reposItem };
      if (key !== projectKey(project)) return { key, parent: projectKey(project) };
      return {
        key,
        parent: (useTree() && parents().projects.get(project)) || ROOT_KEY,
        open: expanded.isOpen(project),
        button: true,
        move: folderUi(),
        onMove: () => moveProject(project),
        onOpen: () => expanded.open(project),
        onClose: () => expanded.close(project),
        onActivate: () => activate(project),
      };
    });

  /** One project row, open to show its pages. */
  const ProjectItem = (props: { p: Project }) => {
    const p = props.p;
    const open = () => expanded.isOpen(p.name);
    const current = () => urlProject() === p.name;
    // The colour of all its agents, in the label and title too (not colour only).
    const health = createMemo(() => projectHealth(agents.inProject(p.name)));
    const label = () => projectHealthLabel(p.name, health());
    return (
      <li
        role="treeitem"
        aria-expanded={open()}
        aria-selected={current()}
        aria-label={label()}
        data-key={projectKey(p.name)}
        data-project={p.name}
        aria-keyshortcuts={folderUi() ? "m" : undefined}
        tabIndex={tabIndex(projectKey(p.name))}
        onFocus={(e) => {
          if (e.target === e.currentTarget) setFocusKey(projectKey(p.name));
        }}
      >
        <div
          class={`tree-row${current() ? " current" : ""}`}
          draggable={folderUi()}
          onDragStart={dragStart({ kind: "project", name: p.name }, p.name)}
          onDragEnd={dragEnd}
          onClick={() => activate(p.name)}
        >
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
          <ProjectDot level={health().level} />
          <span class="tree-name" title={label()}>
            {p.name}
          </span>
          <Show when={blocked(p.name) > 0}>
            <span class="nav-count warn" title="Agents blocked or in error in this project">
              {blocked(p.name)}
            </span>
          </Show>
          <Show when={folderUi()}>
            <RowMenu label={`Actions of ${p.name}`} items={[{ label: "Move to folder…", key: "m", run: () => moveProject(p.name) }]} />
          </Show>
          <SettingsCog project={p.name} />
        </div>
        <Show when={open()}>
          <ul role="group" class="tree-group">
            <For each={PROJECT_ITEMS}>
              {(item) => (
                <Switch
                  fallback={
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
                  }
                >
                  <Match when={item.path === "agents"}>
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
                      onAdd={() => setAdding({ kind: "agent", project: p.name })}
                    />
                  </Match>
                  <Match when={item.path === "repos"}>
                    <ProjectReposNode
                      project={p.name}
                      label={item.label}
                      nav={nav}
                      repos={visibleRepos(p.name)}
                      currentRepo={urlRepo()}
                      query={repoQueries()[p.name] ?? ""}
                      onQuery={(q) => setRepoQueries((all) => ({ ...all, [p.name]: q }))}
                      onAdd={() => setAdding({ kind: "repo", project: p.name })}
                    />
                  </Match>
                </Switch>
              )}
            </For>
          </ul>
        </Show>
      </li>
    );
  };

  /** One folder row, open to show its subfolders and projects. */
  const FolderItem = (props: { id: string }) => {
    const entry = () => entries().get(folderKey(props.id));
    const f = () => (entry() as Extract<TreeEntry, { kind: "folder" }> | undefined)?.folder;
    const key = folderKey(props.id);
    const node = folderNode(props.id);
    const open = () => expanded.isOpen(node);
    const count = () => {
      const e = entry();
      return e ? projectCount(e) : 0;
    };
    const drop = dropTarget(() => props.id);
    return (
      <Show when={f()}>
        {(folder) => (
          <li
            role="treeitem"
            aria-expanded={open()}
            aria-label={`${folder().name}, folder, ${count()} project${count() === 1 ? "" : "s"}`}
            aria-keyshortcuts="+ m F2 Delete"
            data-key={key}
            data-folder={props.id}
            tabIndex={tabIndex(key)}
            onFocus={(e) => {
              if (e.target === e.currentTarget) setFocusKey(key);
            }}
          >
            <div
              class={`tree-row tree-folder${drop.over ? " drop-target" : ""}`}
              draggable={true}
              onDragStart={dragStart({ kind: "folder", id: props.id }, folder().name)}
              onDragEnd={dragEnd}
              onDragOver={drop.onDragOver}
              onDragLeave={drop.onDragLeave}
              onDrop={drop.onDrop}
              onClick={() => {
                expanded.toggle(node);
                setFocusKey(key);
              }}
            >
              <span class={`tree-chevron${open() ? " open" : ""}`} aria-hidden="true">
                ▸
              </span>
              <FolderIcon open={open()} />
              <span class="tree-name" title={folderPath(projectFolders.folders(), props.id)}>
                {folder().name}
              </span>
              <span class="nav-count" title="Projects in this folder and its subfolders">
                {count()}
              </span>
              <RowMenu label={`Actions of the folder ${folder().name}`} items={folderItems(folder())} />
            </div>
            <Show when={open()}>
              <ul role="group" class="tree-group">
                <Entries keys={(entry() as Extract<TreeEntry, { kind: "folder" }>).children.map(entryKey)} />
                <Show when={(entry() as Extract<TreeEntry, { kind: "folder" }>).children.length === 0}>
                  <li class="tree-empty muted small">Empty folder.</li>
                </Show>
              </ul>
            </Show>
          </li>
        )}
      </Show>
    );
  };

  /** The rows of one level of the folder tree, by row key, so that a row keeps its DOM when the tree is built again. */
  const Entries = (props: { keys: string[] }) => (
    <For each={props.keys}>
      {(k) => (
        <Show
          when={k.startsWith("f:")}
          fallback={
            <Show when={projectByName().get(k.slice(2))}>
              {(p) => <ProjectItem p={p()} />}
            </Show>
          }
        >
          <FolderItem id={k.slice(2)} />
        </Show>
      )}
    </For>
  );

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
          badge={
            <Show when={folderUi()}>
              <NewFolderButton onAdd={() => setFolderDialog({ kind: "new", parent: null })} />
            </Show>
          }
          drop={folderUi() ? rootDrop : undefined}
          showFilter={showFilter()}
          query={query()}
          onQuery={setQuery}
        >
          <Show
            when={useTree()}
            fallback={
              <For each={visible()} fallback={<li class="tree-empty muted small">{query() ? "No project matches." : "No projects."}</li>}>
                {(p) => <ProjectItem p={p} />}
              </For>
            }
          >
            <Entries keys={tree().map(entryKey)} />
            <Show when={tree().length === 0}>
              <li class="tree-empty muted small">No projects.</li>
            </Show>
          </Show>
          <Bookmarks nav={nav} />
        </TreeRoot>
      </ul>
      <ErrorBanner error={projectFolders.error()} onDismiss={projectFolders.clearError} />
      <Show when={folderDialog()}>
        {(d) => {
          const close = () => setFolderDialog(null);
          const v = d();
          if (v.kind === "new")
            return (
              <FolderNameDialog
                title={v.parent ? `New folder in ${folderPath(projectFolders.folders(), v.parent)}` : "New folder"}
                save="Add folder"
                onSave={async (name) => {
                  if (v.parent) expanded.open(folderNode(v.parent));
                  await projectFolders.create(name, v.parent);
                }}
                onClose={close}
              />
            );
          if (v.kind === "rename")
            return <FolderNameDialog title={`Rename ${v.folder.name}`} initial={v.folder.name} save="Rename" onSave={(name) => projectFolders.rename(v.folder.id, name)} onClose={close} />;
          if (v.kind === "delete")
            return <DeleteFolderDialog name={v.folder.name} projects={v.projects} onDelete={() => projectFolders.remove(v.folder.id)} onClose={close} />;
          return (
            <MoveToFolderDialog
              what={v.what}
              label={v.label}
              folders={projectFolders.folders()}
              placements={projectFolders.placements()}
              onMove={(to) => moveTo(v.what, to)}
              onClose={close}
            />
          );
        }}
      </Show>
      <Show when={adding()}>
        {(a) => (
          <Switch>
            <Match when={a().kind === "agent"}>
              <NewAgentDialog
                project={a().project}
                onClose={() => setAdding(null)}
                onSaved={(agent) => {
                  setAdding(null);
                  expanded.open(projectAgentsNode(agent.project));
                  navigate(`/agents/${encodeURIComponent(agent.id)}`);
                }}
              />
            </Match>
            <Match when={a().kind === "repo"}>
              <AddRepoDialog
                project={a().project}
                onClose={() => setAdding(null)}
                onAdded={(r) => {
                  setAdding(null);
                  expanded.open(r.project);
                  expanded.open(projectReposNode(r.project));
                }}
              />
            </Match>
          </Switch>
        )}
      </Show>
    </div>
  );
}

/**
 * The "+" of a node row (Agents, Repos): it opens the add dialog of the
 * project. As the cog, it is not a tree row, so it is not in the roving
 * tabindex (tabindex -1). The keyboard runs it with the `+` key on the node
 * row (aria-keyshortcuts). The pages /p/:project/agents and /p/:project/repos
 * also have the add buttons.
 */
function AddButton(props: { label: string; onAdd: () => void }) {
  return (
    <button
      type="button"
      class="tree-add"
      tabIndex={-1}
      aria-label={props.label}
      title={`${props.label} (+)`}
      onClick={(e) => {
        // A click adds; it does not open or close the node.
        e.stopPropagation();
        props.onAdd();
      }}
    >
      <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true">
        <path d="M8 3v10M3 8h10" />
      </svg>
    </button>
  );
}

/**
 * The "+" of the Projects row: a new folder at the top level. It sits in the
 * root link, so a click must not follow the link. Not a Tab stop: the `+`
 * key on the Projects row does the same.
 */
function NewFolderButton(props: { onAdd: () => void }) {
  return (
    <button
      type="button"
      class="tree-add"
      tabIndex={-1}
      aria-label="New folder"
      title="New folder (+)"
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        props.onAdd();
      }}
    >
      <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" aria-hidden="true">
        <path d="M8 3v10M3 8h10" />
      </svg>
    </button>
  );
}

/** A folder, open or closed. Decoration only. */
function FolderIcon(props: { open: boolean }) {
  return (
    <svg class="tree-folder-icon" viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round" aria-hidden="true">
      {props.open ? <path d="M1.5 4.5v8h11l2-5.5h-11l-2 5.5M1.5 4.5V3h4l1.5 1.5h5V7" /> : <path d="M1.5 3h4l1.5 1.5h7.5v8h-13z" />}
    </svg>
  );
}

/**
 * The cog of a project row: a link to the project settings screen
 * (/p/:project/settings, PROJECT_SETTINGS_TABS), active on each of its tabs.
 * It is not a tree row, so it is not in the roving tabindex (tabindex -1):
 * the tree keeps one Tab stop, and the arrow keys move only between rows.
 * The keyboard gets to the screen through the Settings link of the project
 * cards (/projects). Always in view, but muted until the row is under the
 * pointer, has the focus, or the screen is open.
 */
function SettingsCog(props: { project: string }) {
  const label = () => `Settings of ${props.project}`;
  return (
    <A
      href={href(props.project, "settings")}
      activeClass="active"
      class="tree-cog"
      tabIndex={-1}
      aria-label={label()}
      title={label()}
      // A click opens the screen; it does not open or close the project.
      onClick={(e) => e.stopPropagation()}
    >
      <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" aria-hidden="true">
        <path d="M6.6 1.5h2.8l.4 1.9 1.3.7 1.8-.7 1.4 2.4-1.5 1.3v1.6l1.5 1.3-1.4 2.4-1.8-.7-1.3.7-.4 1.9H6.6l-.4-1.9-1.3-.7-1.8.7-1.4-2.4 1.5-1.3V7.1L1.7 5.8l1.4-2.4 1.8.7 1.3-.7z" />
        <circle cx="8" cy="8" r="2" />
      </svg>
    </A>
  );
}

/** The Agents node of a project: the row only toggles; open, it shows the agents. */
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
  onAdd: () => void;
}) {
  const node = () => projectAgentsNode(props.project);
  return (
    <TreeNode
      label={props.label}
      ariaLabel={props.blocked > 0 ? `${props.label}, ${props.blocked} blocked or in error` : props.label}
      rowKey={itemKey(props.project, "agents")}
      project={props.project}
      groupId={`project-agents-${props.project}`}
      open={expanded.isOpen(node())}
      onToggle={() => expanded.toggle(node())}
      nav={props.nav}
      badge={
        <Show when={props.blocked > 0}>
          <span class="nav-count warn" title="Agents blocked or in error in this project">
            {props.blocked}
          </span>
        </Show>
      }
      action={<AddButton label={`Add an agent to ${props.project}`} onAdd={props.onAdd} />}
      actionKey="+"
      showFilter={props.total > FILTER_THRESHOLD}
      query={props.query}
      onQuery={props.onQuery}
    >
      <For each={props.agents} fallback={<li class="tree-empty muted small">{props.query ? "No agent matches." : "No agents."}</li>}>
        {(a) => <AgentRow agent={a} rowKey={projectAgentKey(a.id)} nav={props.nav} project={props.project} current={a.id === props.currentAgent} />}
      </For>
    </TreeNode>
  );
}

/** The Repos node of a project: the row only toggles; open, it loads and shows the repos, each a link to its browser. */
function ProjectReposNode(props: {
  project: string;
  label: string;
  nav: ReturnType<typeof createRovingTree>;
  /** The repos to show: sorted and filtered. */
  repos: Repo[];
  /** The row key of the repo of the URL, or null. */
  currentRepo: string | null;
  query: string;
  onQuery: (q: string) => void;
  onAdd: () => void;
}) {
  const node = () => projectReposNode(props.project);
  const open = () => expanded.isOpen(node());
  createEffect(() => {
    if (open()) repos.load(props.project);
  });
  const empty = () => (props.query ? "No repo matches." : repos.loaded(props.project) ? "No repos." : "Loading…");
  return (
    <TreeNode
      label={props.label}
      rowKey={itemKey(props.project, "repos")}
      project={props.project}
      groupId={`project-repos-${props.project}`}
      open={open()}
      onToggle={() => expanded.toggle(node())}
      nav={props.nav}
      action={<AddButton label={`Add a repo to ${props.project}`} onAdd={props.onAdd} />}
      actionKey="+"
      showFilter={repos.inProject(props.project).length > FILTER_THRESHOLD}
      query={props.query}
      onQuery={props.onQuery}
    >
      <For each={props.repos} fallback={<li class="tree-empty muted small">{empty()}</li>}>
        {(r) => {
          const key = projectRepoKey(props.project, r.name);
          return (
            <li role="none">
              <A
                href={repoHref(props.project, r.name)}
                activeClass="active"
                class="tree-repo"
                aria-current={key === props.currentRepo ? "page" : undefined}
                role="treeitem"
                title={r.url}
                data-key={key}
                data-project={props.project}
                tabIndex={props.nav.tabIndex(key)}
                onFocus={() => props.nav.setFocusKey(key)}
              >
                <span class="tree-name">{r.name}</span>
              </A>
            </li>
          );
        }}
      </For>
    </TreeNode>
  );
}
