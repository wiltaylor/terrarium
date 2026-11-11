// @vitest-environment jsdom
// The Repos tab does what the sidebar Repos node did (TER-153): the list and Add repo,
// for a project name that the URL encodes.
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";

const repos = vi.fn(async (_p: string) => [{ id: "R1", name: "infra", project: "my app", url: "https://example.com/infra.git" }]);
const targets = vi.fn(async (_p: string) => []);
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores(({
  api: { projects: { repos, targets } },
  repos: { inProject: () => [] },
})); });
vi.mock("./AddRepoForm", () => ({
  AddRepoForm: (p: { project: string }) => {
    const d = document.createElement("form");
    d.textContent = `add repo to ${p.project}`;
    return d;
  },
}));

const { default: ReposPage } = await import("./ReposPage");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.replaceChildren();
});

function mount(path: string) {
  const root = document.createElement("div");
  document.body.append(root);
  const history = createMemoryHistory();
  history.set({ value: path });
  dispose = render(
    () =>
      createComponent(MemoryRouter, {
        history,
        get children() {
          return createComponent(Route, { path: "/p/:project/repos", component: ReposPage });
        },
      }),
    root,
  );
  return root;
}

describe("ReposPage", () => {
  it("lists the repos of the decoded project and opens Add repo for it", async () => {
    const root = mount("/p/my%20app/repos");
    await waitFor(() => expect(root.querySelector('a[href="/p/my%20app/repos/infra"]')).toBeTruthy());
    expect(repos).toHaveBeenCalledWith("my app");
    expect(targets).toHaveBeenCalledWith("my app");
    const add = Array.from(root.querySelectorAll("button")).find((b) => b.textContent === "Add repo")!;
    add.click();
    await waitFor(() => expect(root.querySelector("form")?.textContent).toBe("add repo to my app"));
    expect(add.getAttribute("aria-expanded")).toBe("true");
  });
});
