// @vitest-environment jsdom
// The Add repo form: the body it sends, and the new repo goes into the repos
// store (the sidebar Repos node) and to the caller.
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";

const attachRepo = vi.fn();
const added = vi.fn();
vi.mock("../../stores", () => ({ api: { projects: { attachRepo } }, repos: { added } }));

const { AddRepoForm, AddRepoDialog, newRepoBody } = await import("./AddRepoForm");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  vi.clearAllMocks();
});

const mount = (fn: () => unknown) => {
  const el = document.createElement("div");
  document.body.append(el);
  dispose = render(fn as () => Element, el);
  return el;
};

const type = (input: HTMLInputElement, v: string) => {
  input.value = v;
  input.dispatchEvent(new InputEvent("input", { bubbles: true }));
};

describe("newRepoBody", () => {
  it("trims the fields and keeps only the ones with text", () => {
    expect(newRepoBody({ url: " https://x/a.git ", name: " ", default_branch: "main ", path: "" })).toEqual({ url: "https://x/a.git", default_branch: "main" });
  });
});

describe("AddRepoForm", () => {
  it("attaches the repo, puts it into the repos store, and gives it to the caller", async () => {
    const repo = { id: "r9", project: "alpha", name: "infra", url: "https://x/infra.git" };
    attachRepo.mockResolvedValue(repo);
    const onAdded = vi.fn();
    const el = mount(() => createComponent(AddRepoForm, { project: "alpha", onAdded }));
    const form = el.querySelector<HTMLFormElement>('form[aria-label="Add a repo"]')!;
    expect(form.querySelector("h2")?.textContent).toBe("Add a repo");
    type(form.querySelector<HTMLInputElement>("input")!, "https://x/infra.git");
    form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await waitFor(() => expect(onAdded).toHaveBeenCalledWith(repo));
    expect(attachRepo).toHaveBeenCalledWith("alpha", { url: "https://x/infra.git" });
    expect(added).toHaveBeenCalledWith(repo);
  });

  it("shows the error and does not call the caller when the daemon refuses", async () => {
    attachRepo.mockRejectedValue(new Error("repo `infra` exists"));
    const onAdded = vi.fn();
    const el = mount(() => createComponent(AddRepoForm, { project: "alpha", onAdded }));
    const form = el.querySelector<HTMLFormElement>("form")!;
    type(form.querySelector<HTMLInputElement>("input")!, "https://x/infra.git");
    form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await waitFor(() => expect(el.textContent).toContain("exists"));
    expect(onAdded).not.toHaveBeenCalled();
    expect(added).not.toHaveBeenCalled();
  });

  it("has no card and no heading in the dialog, which names the project", () => {
    mount(() => createComponent(AddRepoDialog, { project: "alpha", onClose: () => {}, onAdded: () => {} }));
    const dialog = document.querySelector<HTMLElement>('[role="dialog"]')!;
    expect(dialog.getAttribute("aria-label")).toBe("Add a repo to alpha");
    const form = dialog.querySelector<HTMLFormElement>("form")!;
    expect(form.classList.contains("card-box")).toBe(false);
    expect(form.querySelector("h2")).toBeNull();
  });
});
