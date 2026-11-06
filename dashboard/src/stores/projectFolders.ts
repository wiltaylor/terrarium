// Project folders of the sidebar (docs/api.md §6.12, TER-48). The folders
// are on the daemon, so they are the same in each browser. Only the operator
// and a sysadmin may read them: for any other caller `available()` stays
// false and the tree shows the plain project list.

import { createSignal } from "solid-js";
import type { Api } from "../api/client";
import { ApiError } from "../api/http";
import type { ProjectFolder, ProjectPlacement } from "../api/types";
import type { Moving } from "../features/projectFolders/model";

export function createProjectFoldersStore(api: Pick<Api, "projectFolders">) {
  const [folders, setFolders] = createSignal<ProjectFolder[]>([]);
  const [placements, setPlacements] = createSignal<ProjectPlacement[]>([]);
  const [available, setAvailable] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  // Each answer carries the whole tree; a late answer must not undo a newer write.
  let gen = 0;

  const refresh = async () => {
    const mine = ++gen;
    try {
      const t = await api.projectFolders.list();
      if (mine !== gen) return;
      setFolders(t.folders);
      setPlacements(t.placements);
      setAvailable(true);
    } catch (e) {
      if (mine !== gen) return;
      if (e instanceof ApiError && (e.status === 403 || e.status === 404)) setAvailable(false);
      else setError(e);
    }
  };

  /** Run a write, then load the tree again. On an error the error banner shows it, and the tree is the daemon's. */
  const write = async <T>(fn: () => Promise<T>): Promise<T | undefined> => {
    try {
      const out = await fn();
      await refresh();
      return out;
    } catch (e) {
      setError(e);
      await refresh();
      return undefined;
    }
  };

  /** Move a project or a folder into `target` (a folder ID, or null for the root), at the end. */
  const move = (what: Moving, target: string | null) => {
    // At once: the row moves before the answer comes.
    gen++;
    if (what.kind === "project") {
      const rest = placements().filter((p) => p.project !== what.name);
      const end = rest.filter((p) => p.folder_id === target).length;
      setPlacements([...rest, { project: what.name, folder_id: target, position: end }]);
      return write(() => api.projectFolders.place(what.name, target));
    }
    const end = folders().filter((f) => f.parent_id === target && f.id !== what.id).length;
    setFolders(folders().map((f) => (f.id === what.id ? { ...f, parent_id: target, position: end } : f)));
    return write(() => api.projectFolders.update(what.id, { parent_id: target }));
  };

  return {
    folders,
    placements,
    available,
    error,
    clearError: () => setError(null),
    refresh,
    create: (name: string, parentId: string | null) => write(() => api.projectFolders.create({ name, parent_id: parentId })),
    rename: (id: string, name: string) => write(() => api.projectFolders.update(id, { name })),
    remove: (id: string) => write(() => api.projectFolders.remove(id)),
    move,
  };
}

export type ProjectFoldersStore = ReturnType<typeof createProjectFoldersStore>;
