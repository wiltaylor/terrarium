import { describe, expect, it, vi } from "vitest";
import type { Repo } from "../api/types";
import { createReposStore } from "./repos";

const repo = (project: string, name: string): Repo => ({ id: `${project}/${name}`, project, name, url: `https://x/${name}.git` }) as Repo;

describe("createReposStore", () => {
  it("loads the repos of a project once, sorted by name", async () => {
    const list = vi.fn(async () => [repo("alpha", "web"), repo("alpha", "infra")]);
    const st = createReposStore({ projects: { repos: list } });
    expect(st.loaded("alpha")).toBe(false);
    expect(st.inProject("alpha")).toEqual([]);
    st.load("alpha");
    st.load("alpha");
    await vi.waitFor(() => expect(st.loaded("alpha")).toBe(true));
    expect(list).toHaveBeenCalledTimes(1);
    expect(st.inProject("alpha").map((r) => r.name)).toEqual(["infra", "web"]);
  });

  it("puts an added repo into the list of its project at once, with no duplicate", async () => {
    const st = createReposStore({ projects: { repos: async () => [repo("alpha", "web")] } });
    await st.refresh("alpha");
    st.added(repo("alpha", "api"));
    st.added(repo("alpha", "api"));
    expect(st.inProject("alpha").map((r) => r.name)).toEqual(["api", "web"]);
    st.added(repo("beta", "x"));
    expect(st.inProject("beta").map((r) => r.name)).toEqual(["x"]);
  });

  it("keeps the error of a failed load", async () => {
    const st = createReposStore({ projects: { repos: async () => Promise.reject(new Error("down")) } });
    await st.refresh("alpha");
    expect(st.state.errors.alpha).toBeInstanceOf(Error);
    expect(st.loaded("alpha")).toBe(false);
  });
});
