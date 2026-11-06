// The project folders of the mock follow the daemon (docs/api.md §6.12,
// crates/terrariumd/tests/project_folders.rs).
import { describe, expect, it } from "vitest";
import { createMockApi, type MockOptions } from ".";
import type { ProjectFolderTree } from "../types";

const mock = (o: Partial<MockOptions> = {}) => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, ...o });

const names = (t: ProjectFolderTree) => {
  const name = (id: string | null) => t.folders.find((f) => f.id === id)?.name ?? "";
  return t.folders.map((f) => `${f.name}(${name(f.parent_id)})@${f.position}`).sort();
};
const places = (t: ProjectFolderTree) => t.placements.map((p) => `${p.project}@${p.folder_id ?? "-"}:${p.position}`).sort();

describe("mock project folders", () => {
  it("nests, moves, refuses a cycle, and moves the content up on a delete", async () => {
    const api = mock();
    expect(await api.projectFolders.list()).toEqual({ folders: [], placements: [] });
    const work = await api.projectFolders.create({ name: "  Work " });
    expect(work.name).toBe("Work");
    const infra = await api.projectFolders.create({ name: "Infra", parent_id: work.id });
    await api.projectFolders.create({ name: "Home" });
    await expect(api.projectFolders.create({ name: " " })).rejects.toMatchObject({ status: 422 });
    await expect(api.projectFolders.update(work.id, { parent_id: infra.id })).rejects.toMatchObject({ status: 422 });
    await expect(api.projectFolders.update(work.id, { parent_id: work.id })).rejects.toMatchObject({ status: 422 });

    await api.projectFolders.place("terrarium", infra.id);
    await api.projectFolders.place("webshop", work.id);
    expect(names(await api.projectFolders.list())).toEqual(["Home()@1", "Infra(Work)@0", "Work()@0"]);
    await expect(api.projectFolders.place("nope", infra.id)).rejects.toMatchObject({ status: 404 });

    await api.projectFolders.remove(infra.id);
    const t = await api.projectFolders.list();
    expect(names(t)).toEqual(["Home()@1", "Work()@0"]);
    expect(places(t)).toEqual([`terrarium@${work.id}:1`, `webshop@${work.id}:0`]);
  });

  it("refuses a folder deeper than 8 levels", async () => {
    const api = mock();
    let parent: string | null = null;
    for (let i = 1; i <= 8; i++) parent = (await api.projectFolders.create({ name: `L${i}`, parent_id: parent })).id;
    await expect(api.projectFolders.create({ name: "L9", parent_id: parent })).rejects.toMatchObject({ status: 422 });
  });

  it("is for the operator and a sysadmin only", async () => {
    await expect(mock({ scheduleCaller: { role: "member", project: "terrarium" } }).projectFolders.list()).rejects.toMatchObject({ status: 403 });
    await expect(mock({ scheduleCaller: { role: "sysadmin" } }).projectFolders.list()).resolves.toEqual({ folders: [], placements: [] });
  });
});
