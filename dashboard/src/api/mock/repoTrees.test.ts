import { describe, expect, it } from "vitest";
import { createMockApi } from "./index";

describe("the mock repo browser", () => {
  it("has main and two agent trees, folders first, and the daemon's path rules", async () => {
    const api = createMockApi();
    const [repo] = await api.projects.repos("terrarium");
    const t = await api.repos.trees(repo!.id);
    expect(t.trees.map((x) => x.id)).toEqual(["main", "agent:01JB0WRK100000000000000002", "agent:01JB0WRK200000000000000003"]);
    expect(t.trees[1]!.label).toBe("runner-dev · feat/runner-backoff");
    const root = await api.repos.files(repo!.id, "main", "");
    expect(root.entries.slice(0, 2).map((e) => e.kind)).toEqual(["dir", "dir"]);
    await expect(api.repos.files(repo!.id, "main", "../x")).rejects.toMatchObject({ status: 422 });
    await expect(api.repos.file(repo!.id, "main", ".git/config")).rejects.toMatchObject({ status: 422 });
    const link = await api.repos.files(repo!.id, "agent:01JB0WRK100000000000000002", "");
    expect(link.entries.find((e) => e.name === "secrets")).toMatchObject({ kind: "symlink", target: "/etc/passwd" });
    const f = await api.repos.file(repo!.id, "main", "docs/logo.png");
    expect(f.text).toBe(false);
    const changes = await api.repos.diff(repo!.id, "agent:01JB0WRK100000000000000002");
    expect(changes.committed.files.length).toBe(2);
    expect(changes.uncommitted?.untracked).toEqual(["notes.txt", "scratch/out.log"]);
    const p1 = await api.repos.commits(repo!.id, "main");
    expect(p1.items.length).toBe(5);
    const p2 = await api.repos.commits(repo!.id, "main", p1.next);
    expect(p2.next).toBeNull();
    expect((await api.repos.commit(repo!.id, "main", p1.items[0]!.sha)).diff.files.length).toBe(1);
  });
});
