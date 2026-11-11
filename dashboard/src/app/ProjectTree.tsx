import { useInstance } from "../instance";
// The sidebar project tree: a Projects root that links the project cards,
// and under it one row per project. Each project row links its tabbed
// project page. Keyboard follows the WAI-ARIA tree pattern with a roving
// tabindex. A health dot before each project name gives the state of its
// agents, and a cog at the right of each project row opens its settings.
//
// Project folders (docs/api.md §6.12, TER-48): for the operator and a
// sysadmin, folders group the projects, nested. A "+" on the Projects row
// adds a folder, the "⋯" of a row has Move to folder (and for a folder New
// subfolder, Rename, and Delete), and a project or folder row can be dragged
// onto a folder row or onto the Projects row (the top level). With a filter
// typed, the tree shows the plain list of matching projects.

import { A, useLocation, useNavigate } from "@solidjs/router";
import { createEffect, createMemo, createSignal, For, onMount, Show, untrack } from "solid-js";
import type { Project, ProjectFolder } from "../api/types";
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
import { projectHealth, projectHealthLabel } from "../lib/projectHealth";

import { expanded, ROOT_KEY, TreeRoot, type DropTarget } from "./TreeRoot";
import { filterProjects, FILTER_THRESHOLD, agentFromPath, projectFromPath, projectKey, PROJECTS_ROOT, projectTreeKeys } from "./treeModel";
import { createRovingTree } from "./treeNav";
import { Bookmarks, BOOKMARKS_KEY, bookmarkKey } from "./Bookmarks";


const projectHref = (name: string) => `/p/${encodeURIComponent(name)}`;
const settingsHref = (name: string) => `${projectHref(name)}/settings`;

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
  const { agents, projectFolders, projects, bookmarks } = useInstance();
  const location = useLocation();
  const navigate = useNavigate();
  const [query, setQuery] = createSignal("");

  const urlProject = () => projectFromPath(location.pathname);
  // An agent page takes an ID or `project/name` (agents.byId).
  const urlAgentProject = () => {
    const id = agentFromPath(location.pathname);
    return id ? agents.byId(id)?.project : undefined;
  };
  const currentProject = () => urlProject() ?? urlAgentProject() ?? null;
  const rootOpen = () => expanded.isOpen(PROJECTS_ROOT);

  // A project page or an agent page opens the Projects root.
  createEffect(() => expanded.follow(location.pathname, (id) => agents.byId(id)?.project));
  // Drop legacy project open names and folders that are gone.
  createEffect(() => expanded.prune(projectFolders.available() ? projectFolders.folders().map((f) => f.id) : null));

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
    const p = currentProject();
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

  // The rows in screen order, for the roving tabindex.
  const treeKeys = (rootOpen: boolean, names: string[]) => projectTreeKeys({ rootKey: ROOT_KEY, rootOpen, projects: names });
  const keys = createMemo(() => {
    const base = !rootOpen() || !useTree() ? treeKeys(rootOpen(), visible().map((p) => p.name)) : [ROOT_KEY, ...folderTreeKeys(tree(), expanded.isOpen, (p) => treeKeys(true, [p]).slice(1))];
    return rootOpen() ? [...base, BOOKMARKS_KEY, ...(expanded.isOpen(BOOKMARKS_KEY) ? bookmarks.items().map((b) => bookmarkKey(b.href)) : [])] : base;
  });
  // Tab reaches the last focused row, else the current project, else the root.
  const nav = createRovingTree({
    keys,
    preferred: () => {
      const p = currentProject();
      return p ? projectKey(p) : null;
    },
  });
  const tabIndex = nav.tabIndex;
  const setFocusKey = nav.setFocusKey;

  const blocked = (name: string) => agents.inProject(name).filter((a) => a.status === "blocked" || a.status === "error").length;

  /** Open the tabbed project page from Enter, Space, or a click on its tree row. */
  const activate = (name: string) => {
    setFocusKey(projectKey(name));
    navigate(projectHref(name));
  };

  const onKeyDown = (e: KeyboardEvent) =>
    nav.onKeyDown(e, (target) => {
      const key = target.dataset.key;
      if (!key) return null;
      if (key === BOOKMARKS_KEY) return {
        key, parent: ROOT_KEY, button: true, edit: true, open: expanded.isOpen(BOOKMARKS_KEY),
        onRename: () => navigate("/bookmarks"),
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
      if (key !== projectKey(project)) return null;
      return {
        key,
        parent: (useTree() && parents().projects.get(project)) || ROOT_KEY,
        button: true,
        move: folderUi(),
        onMove: () => moveProject(project),
        onActivate: () => activate(project),
      };
    });

  /** One project row, linked to its tabbed project page. */
  const ProjectItem = (props: { p: Project }) => {
    const p = props.p;
    const current = () => currentProject() === p.name;
    // The colour of all its agents, in the label and title too (not colour only).
    const health = createMemo(() => projectHealth(agents.inProject(p.name)));
    const label = () => projectHealthLabel(p.name, health());
    return (
      <li
        role="treeitem"
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
          onClick={(e) => {
            if (!(e.target as HTMLElement).closest("a, button")) activate(p.name);
          }}
        >
          <ProjectDot level={health().level} />
          <A
            href={projectHref(p.name)}
            class="tree-project-link"
            aria-current={current() ? "page" : undefined}
            tabIndex={-1}
            onClick={() => setFocusKey(projectKey(p.name))}
          >
            <span class="tree-name" title={label()}>
              {p.name}
            </span>
            <Show when={blocked(p.name) > 0}>
              <span class="nav-count warn" title="Agents blocked or in error in this project">
                {blocked(p.name)}
              </span>
            </Show>
          </A>
          <Show when={folderUi()}>
            <RowMenu label={`Actions of ${p.name}`} items={[{ label: "Move to folder…", key: "m", run: () => moveProject(p.name) }]} />
          </Show>
          <SettingsCog project={p.name} />
        </div>
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
    </div>
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
      href={settingsHref(props.project)}
      activeClass="active"
      class="tree-cog"
      tabIndex={-1}
      aria-label={label()}
      title={label()}
      onClick={(e) => e.stopPropagation()}
    >
      <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" aria-hidden="true">
        <path d="M6.6 1.5h2.8l.4 1.9 1.3.7 1.8-.7 1.4 2.4-1.5 1.3v1.6l1.5 1.3-1.4 2.4-1.8-.7-1.3.7-.4 1.9H6.6l-.4-1.9-1.3-.7-1.8.7-1.4-2.4 1.5-1.3V7.1L1.7 5.8l1.4-2.4 1.8.7 1.3-.7z" />
        <circle cx="8" cy="8" r="2" />
      </svg>
    </A>
  );
}
