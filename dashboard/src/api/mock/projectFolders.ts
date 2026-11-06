// The project folders of the mock (docs/api.md §6.12), after the daemon's
// `store/project_folders.rs`: folders nest at most 8 levels, a move into
// the folder itself or a subfolder is refused, positions are renumbered
// per parent on each write, and a delete moves the content up one level.
// The operator and a sysadmin only. The seed has no folders.

import type { ProjectFolder, ProjectFolderTree, ProjectPlacement } from "../types";
import type { ScheduleCaller } from "./schedules";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export const MAX_FOLDER_DEPTH = 8;

export interface ProjectFolderMockContext {
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => unknown;
  /** Throws `404` for an unknown project. */
  project: (name: string) => unknown;
  caller?: ScheduleCaller;
  ulid: (tag: string) => string;
  iso: () => string;
}

export function createProjectFolderMock(ctx: ProjectFolderMockContext) {
  let folders: ProjectFolder[] = [];
  let placements: ProjectPlacement[] = [];

  const allowed = () => {
    const c = ctx.caller;
    if (c && c.role !== "operator" && c.role !== "sysadmin") ctx.fail(403, "forbidden", "project folders needs sysadmin or the operator");
  };
  const find = (id: string) => folders.find((f) => f.id === id) ?? ctx.fail(404, "not_found", `no project folder \`${id}\``);
  const cleanName = (name: string) => {
    const n = name.trim();
    if (!n) ctx.fail(422, "invalid", "a folder name cannot be empty");
    if ([...n].length > 64) ctx.fail(422, "invalid", "a folder name has at most 64 characters");
    // eslint-disable-next-line no-control-regex
    if (/[\u0000-\u001f\u007f-\u009f]/.test(n)) ctx.fail(422, "invalid", "a folder name cannot have control characters");
    return n;
  };
  const parentOf = (id: string) => folders.find((f) => f.id === id)?.parent_id ?? null;
  const level = (id: string) => {
    let n = 0;
    for (let at: string | null = id; at && n <= folders.length; at = parentOf(at)) n++;
    return n;
  };
  const height = (id: string): number => 1 + Math.max(0, ...folders.filter((f) => f.parent_id === id).map((f) => height(f.id)));
  const within = (id: string, ancestor: string) => {
    for (let at: string | null = id, n = 0; at && n <= folders.length; at = parentOf(at), n++) if (at === ancestor) return true;
    return false;
  };
  const byPos = <T extends { position: number }>(a: T, b: T) => a.position - b.position;
  const slot = (pos: number | undefined, len: number) => (pos === undefined ? len : Math.max(0, Math.min(len, pos)));

  const orderFolders = (parent: string | null, id: string, pos?: number) => {
    const rest = folders.filter((f) => f.parent_id === parent && f.id !== id).sort(byPos);
    rest.splice(slot(pos, rest.length), 0, find(id));
    rest.forEach((f, i) => (f.position = i));
  };
  const orderProjects = (folder: string | null, project: string, pos?: number) => {
    const rest = placements.filter((p) => p.folder_id === folder && p.project !== project).sort(byPos);
    rest.splice(slot(pos, rest.length), 0, placements.find((p) => p.project === project)!);
    rest.forEach((p, i) => (p.position = i));
  };
  const renumber = (parent: string | null) => {
    folders
      .filter((f) => f.parent_id === parent)
      .sort(byPos)
      .forEach((f, i) => (f.position = i));
    placements
      .filter((p) => p.folder_id === parent)
      .sort(byPos)
      .forEach((p, i) => (p.position = i));
  };
  const tooDeep = () => ctx.fail(422, "invalid", `folders nest at most ${MAX_FOLDER_DEPTH} levels deep`);
  const tree = (): ProjectFolderTree => ({
    folders: folders.map((f) => ({ ...f })),
    placements: placements.map((p) => ({ ...p })),
  });

  return {
    /** The daemon's cascade: a deleted project loses its place. */
    onProjectRemoved: (name: string) => {
      placements = placements.filter((p) => p.project !== name);
    },
    api: {
      list: () =>
        ctx.reply(() => {
          allowed();
          return tree();
        }),
      create: (body: { name: string; parent_id?: string | null }) =>
        ctx.reply(() => {
          allowed();
          const name = cleanName(body.name);
          const parent = body.parent_id || null;
          if (parent) {
            find(parent);
            if (level(parent) + 1 > MAX_FOLDER_DEPTH) tooDeep();
          }
          const at = ctx.iso();
          const f: ProjectFolder = { id: ctx.ulid("folder"), name, parent_id: parent, position: 0, created_at: at, updated_at: at };
          folders.push(f);
          orderFolders(parent, f.id);
          ctx.audit("project_folder.create", f.id, undefined, { name, parent_id: parent });
          return { ...f };
        }),
      update: (id: string, body: { name?: string; parent_id?: string | null; position?: number }) =>
        ctx.reply(() => {
          allowed();
          const f = find(id);
          const name = body.name === undefined ? f.name : cleanName(body.name);
          const parent = body.parent_id === undefined ? f.parent_id : body.parent_id || null;
          const moved = parent !== f.parent_id;
          if (moved && parent) {
            find(parent);
            if (within(parent, id)) ctx.fail(422, "invalid", "a folder cannot move into itself or one of its subfolders");
            if (level(parent) + height(id) > MAX_FOLDER_DEPTH) tooDeep();
          }
          const from = f.parent_id;
          f.name = name;
          f.parent_id = parent;
          f.updated_at = ctx.iso();
          if (moved || body.position !== undefined) {
            orderFolders(parent, id, body.position ?? (moved ? undefined : f.position));
            if (moved) renumber(from);
          }
          ctx.audit("project_folder.update", id, undefined, { name, parent_id: parent, position: body.position ?? null });
          return { ...f };
        }),
      remove: (id: string) =>
        ctx.reply(() => {
          allowed();
          const f = find(id);
          const parent = f.parent_id;
          const children = folders.filter((c) => c.parent_id === id).sort(byPos);
          const order = folders
            .filter((s) => s.parent_id === parent)
            .sort(byPos)
            .flatMap((s) => (s.id === id ? children : [s]));
          order.forEach((s, i) => {
            s.parent_id = parent;
            s.position = i;
          });
          const base = placements.filter((p) => p.folder_id === parent).length;
          const moved = placements.filter((p) => p.folder_id === id).sort(byPos);
          moved.forEach((p, i) => {
            p.folder_id = parent;
            p.position = base + i;
          });
          folders = folders.filter((x) => x.id !== id);
          renumber(parent);
          ctx.audit("project_folder.delete", id, undefined, { name: f.name, parent_id: parent, children: children.length, projects: moved.length });
        }),
      place: (project: string, folderId: string | null, position?: number) =>
        ctx.reply(() => {
          allowed();
          ctx.project(project);
          const folder = folderId || null;
          if (folder) find(folder);
          let p = placements.find((x) => x.project === project);
          const from = p ? p.folder_id : undefined;
          if (!p) {
            p = { project, folder_id: folder, position: 0 };
            placements.push(p);
          }
          p.folder_id = folder;
          orderProjects(folder, project, position);
          if (from !== undefined && from !== folder) renumber(from);
          ctx.audit("project.folder", project, project, { folder_id: folder, position: position ?? null });
          return { ...p };
        }),
    },
  };
}
