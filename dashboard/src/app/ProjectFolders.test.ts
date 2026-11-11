// @vitest-environment jsdom
// Project folders in the sidebar tree (TER-48): folder rows with their
// projects, Move to folder from the row menu, a drop of a project row onto a
// folder row and onto the Projects row, the `m` key, and the arrow keys
// over the folder rows. The folders store runs on the mock API.
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { waitFor } from "../test/wait";

vi.mock("../stores", async () => {
  const { instanceTestStores } = await import("../test/instance");
  const { createMockApi } = await import("../api/mock");
  const { createProjectFoldersStore } = await import("../stores/projectFolders");
  const { createBookmarksStore } = await import("../stores/bookmarks");
  const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
  return instanceTestStores({
    api,
    bookmarks: createBookmarksStore(null, window.location.origin),
    projectFolders: createProjectFoldersStore(api),
    projects: {
      projects: () => [{ name: "terrarium" }, { name: "webshop" }],
      selected: () => null,
      select: () => {},
    },
    agents: { byId: () => undefined, inProject: () => [] },
    repos: { load: () => {}, loaded: () => true, inProject: () => [], added: () => {} },
    pins: { toggle: async () => {} },
  });
});

const stores = (await import("../stores")) as unknown as {
  api: import("../api/client").Api;
  projectFolders: import("../stores/projectFolders").ProjectFoldersStore;
};
const { ProjectTree } = await import("./ProjectTree");
const { useInstance } = await import("../instance");
const { expanded } = useInstance();

/** Wait until `check` gives a truthy value, and give it. */
const want = <T>(check: () => T | null | undefined | false) =>
  waitFor(() => {
    const v = check();
    if (!v) throw new Error("not yet");
    return v;
  });

let dispose: (() => void) | undefined;
let work = "";
let infra = "";

beforeEach(async () => {
  // A fresh tree on the daemon (the mock lives across tests): Work > Infra, and both projects at the top level.
  const t = await stores.api.projectFolders.list();
  for (const p of t.placements) await stores.api.projectFolders.place(p.project, null);
  for (const f of t.folders.filter((x) => x.parent_id === null)) await stores.api.projectFolders.remove(f.id);
  for (const f of (await stores.api.projectFolders.list()).folders) await stores.api.projectFolders.remove(f.id);
  work = (await stores.api.projectFolders.create({ name: "Work" })).id;
  infra = (await stores.api.projectFolders.create({ name: "Infra", parent_id: work })).id;
  for (const n of expanded.list()) if (n.startsWith("folder:")) expanded.close(n);
});

afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
});

const mount = async (path = "/projects") => {
  const el = document.createElement("div");
  document.body.append(el);
  const history = createMemoryHistory();
  history.set({ value: path });
  dispose = render(
    () =>
      createComponent(MemoryRouter, {
        history,
        get children() {
          return createComponent(Route, { path: "*", component: ProjectTree });
        },
      }),
    el,
  );
  await want(() => !!el.querySelector(`[data-key="f:${work}"]`));
  return el;
};

const row = (el: HTMLElement, key: string) => el.querySelector<HTMLElement>(`[role="treeitem"][data-key="${key}"]`);
/** The keys of the rows at the top of the Projects group. */
const top = (el: HTMLElement) => Array.from(el.querySelectorAll<HTMLElement>("#project-tree-root-group > [role='treeitem']")).map((r) => r.dataset.key);
/** The row keys in the group of a folder row. */
const inFolder = (el: HTMLElement, id: string) =>
  Array.from(row(el, `f:${id}`)?.querySelectorAll<HTMLElement>(":scope > [role='group'] > [role='treeitem']") ?? []).map((r) => r.dataset.key);

const fire = (target: Element, type: string) => {
  const e = new Event(type, { bubbles: true, cancelable: true });
  target.dispatchEvent(e);
  return e;
};

describe("project folders in the tree", () => {
  it("shows folders before the projects, nested, with the count and the open state", async () => {
    await stores.api.projectFolders.place("webshop", infra);
    const el = await mount();
    await want(() => top(el).length === 2);
    expect(top(el)).toEqual([`f:${work}`, "p:terrarium"]);
    const w = row(el, `f:${work}`)!;
    expect(w.getAttribute("aria-expanded")).toBe("false");
    expect(w.getAttribute("aria-label")).toBe("Work, folder, 1 project");

    w.querySelector<HTMLElement>(".tree-row")!.click();
    expect(expanded.isOpen(`folder:${work}`)).toBe(true);
    expect(inFolder(el, work)).toEqual([`f:${infra}`]);
    row(el, `f:${infra}`)!.querySelector<HTMLElement>(".tree-row")!.click();
    expect(inFolder(el, infra)).toEqual(["p:webshop"]);
  });

  it("moves a project with Move to folder in its row menu", async () => {
    const el = await mount();
    const p = row(el, "p:terrarium")!;
    p.querySelector<HTMLButtonElement>(".tree-menu-button")!.click();
    const item = Array.from(p.querySelectorAll<HTMLButtonElement>('[role="menuitem"]')).find((b) => b.textContent === "Move to folder…")!;
    item.click();

    const dialog = await want(() => document.querySelector<HTMLElement>('[role="dialog"]'));
    expect(dialog.getAttribute("aria-label")).toBe("Move terrarium to folder");
    const choices = Array.from(dialog.querySelectorAll<HTMLLabelElement>(".folder-choice"));
    expect(choices.map((c) => c.textContent)).toEqual(["Projects (top level)", "Work", "Infra"]);
    // It is at the top level now: that choice is off.
    expect(choices[0]!.querySelector("input")!.disabled).toBe(true);
    choices[2]!.querySelector("input")!.click();
    dialog.querySelector<HTMLFormElement>("form")!.requestSubmit();

    await want(() => stores.projectFolders.placements().some((x) => x.project === "terrarium" && x.folder_id === infra));
    expect((await stores.api.projectFolders.list()).placements).toContainEqual({ project: "terrarium", folder_id: infra, position: 0 });
    // The target and its parents open, so the row stays in view.
    await want(() => inFolder(el, infra).includes("p:terrarium"));
    expect(top(el)).toEqual([`f:${work}`, "p:webshop"]);
    // The dialog closes when the daemon has answered.
    await want(() => !document.querySelector('[role="dialog"]'));
  });

  it("moves a project by a drop onto a folder row, and back by a drop onto the Projects row", async () => {
    const el = await mount();
    const drag = row(el, "p:webshop")!.querySelector<HTMLElement>(".tree-row")!;
    expect(drag.getAttribute("draggable")).toBe("true");
    const target = row(el, `f:${work}`)!.querySelector<HTMLElement>(".tree-row")!;

    fire(drag, "dragstart");
    const over = fire(target, "dragover");
    // preventDefault on dragover is what lets the browser drop here.
    expect(over.defaultPrevented).toBe(true);
    expect(target.classList.contains("drop-target")).toBe(true);
    fire(target, "drop");
    fire(drag, "dragend");
    expect(target.classList.contains("drop-target")).toBe(false);

    await want(() => stores.projectFolders.placements().some((x) => x.project === "webshop" && x.folder_id === work));
    await want(() => inFolder(el, work).includes("p:webshop"));
    expect(inFolder(el, work)).toEqual([`f:${infra}`, "p:webshop"]);

    // A folder cannot drop into its own subfolder: no preventDefault, no target mark.
    const w = row(el, `f:${work}`)!.querySelector<HTMLElement>(".tree-row")!;
    const i = row(el, `f:${infra}`)!.querySelector<HTMLElement>(".tree-row")!;
    fire(w, "dragstart");
    expect(fire(i, "dragover").defaultPrevented).toBe(false);
    fire(w, "dragend");

    // Back to the top level: drop on the Projects row.
    const again = row(el, "p:webshop")!.querySelector<HTMLElement>(".tree-row")!;
    const root = el.querySelector<HTMLElement>('[data-key="root"]')!;
    fire(again, "dragstart");
    expect(fire(root, "dragover").defaultPrevented).toBe(true);
    fire(root, "drop");
    await want(() => top(el).includes("p:webshop"));
    expect((await stores.api.projectFolders.list()).placements.find((x) => x.project === "webshop")?.folder_id).toBeNull();
  });

  it("opens Move to folder with the m key on a project row", async () => {
    const el = await mount();
    const p = row(el, "p:terrarium")!;
    expect(p.getAttribute("aria-keyshortcuts")).toBe("m");
    p.dispatchEvent(new KeyboardEvent("keydown", { key: "m", bubbles: true, cancelable: true }));
    const dialog = await want(() => document.querySelector<HTMLElement>('[role="dialog"]'));
    expect(dialog.getAttribute("aria-label")).toBe("Move terrarium to folder");
  });

  it("moves the focus through the folder rows with the arrow keys", async () => {
    const el = await mount();
    expect(top(el)).toContain(`f:${work}`);
    expanded.close("folder:" + work);
    // The tree keys follow the folder rows: ArrowDown from the root goes to the folder.
    const root = el.querySelector<HTMLElement>('[data-key="root"]')!;
    root.focus();
    root.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true, cancelable: true }));
    expect(document.activeElement?.getAttribute("data-key")).toBe(`f:${work}`);
    (document.activeElement as HTMLElement).dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true, cancelable: true }));
    expect(document.activeElement?.getAttribute("data-key")).toBe(top(el)[1]);
    expect(top(el)[1]).toMatch(/^p:/);
    // ArrowLeft on a top-level project goes to the root.
    (document.activeElement as HTMLElement).dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true, cancelable: true }));
    expect(document.activeElement?.getAttribute("data-key")).toBe("root");
  });
});
