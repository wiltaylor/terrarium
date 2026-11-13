// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import type { Repo } from "../../api/types";
import { waitFor } from "../../test/wait";

const repos = vi.fn(async (project: string): Promise<Repo[]> => [{ id: project, project, name: "infra", url: "https://example.com/repo" }]);
const targets = vi.fn(async () => []);
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ api: { projects: { repos, targets } }, repos: { inProject: () => [] } });
});
vi.mock("./AddRepoForm", () => ({ AddRepoForm: (props: { project: string }) => <form>{props.project}</form> }));
const { ReposPanel } = await import("./ReposPanel");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); vi.clearAllMocks(); });

it("isolates duplicate add state, preserves detail links and ignores late project replies", async () => {
  let resolveOld!: (rows: Repo[]) => void;
  repos.mockImplementationOnce(() => new Promise((resolve) => { resolveOld = resolve; }));
  const [project, setProject] = createSignal("old");
  const root = document.createElement("div"); document.body.append(root);
  const history = createMemoryHistory(); history.set({ value: "/host" });
  dispose = render(() => <MemoryRouter history={history}><Route path="/host" component={() => <>
    <section><ReposPanel project={project()} /></section><section><ReposPanel project="other" /></section>
  </>} /></MemoryRouter>, root);
  await waitFor(() => expect(repos).toHaveBeenCalledWith("old"));
  const sections = root.querySelectorAll("section");
  sections[0]!.querySelector<HTMLButtonElement>("button.primary")!.click();
  expect(sections[0]!.querySelector("form")?.textContent).toBe("old");
  expect(sections[1]!.querySelector("form")).toBeNull();
  setProject("my app");
  await waitFor(() => expect(sections[0]!.querySelector('a[href="/p/my%20app/repos/infra"]')).toBeTruthy());
  resolveOld([{ id: "old", project: "old", name: "stale", url: "old-url" }]);
  await Promise.resolve();
  expect(sections[0]!.textContent).not.toContain("stale");
  expect(sections[0]!.querySelector("form")).toBeNull();
  expect(history.get()).toBe("/host");
});
