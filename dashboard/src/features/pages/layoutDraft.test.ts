import { createRoot } from "solid-js";
import { expect, it, vi } from "vitest";
import { ApiError } from "../../api/http";
import type { AgentEvent, PageWidget, ProjectLayout, ProjectLayoutInput, ProjectLayoutPage } from "../../api/types";
import { addPage, canonicalPages, createLayoutEditor, draftFromLayout, movePage, moveWidget, removePage, renamePage, setPageWidgets } from "./layoutDraft";

const widget = (id: string, y = 0, extra: Partial<PageWidget> = {}): PageWidget => ({ id, kind: "board", x: 0, y, w: 12, h: 3, options: { view: "lanes" }, ...extra });
const page = (slug: string, position: number, widgets: PageWidget[] = []): ProjectLayoutPage => ({ slug, title: slug.toUpperCase(), position, widgets });
const snapshot = (): ProjectLayout => ({ revision: 4, pages: [
  { ...page("b", 1, [widget("w2")]), project: "demo", updated_at: "", updated_by: "operator", builtin: "board" },
  { ...page("a", 0, [widget("w1", 0, { options: { view: "done", search: "x" } })]), project: "demo", updated_at: "", updated_by: "operator" },
] });

it("starts a draft in tab order with only input fields, without changing the snapshot", () => {
  const layout = snapshot();
  const before = structuredClone(layout);
  const draft = draftFromLayout(layout);
  expect(draft.map(p => [p.slug, p.position])).toEqual([["a", 0], ["b", 1]]);
  expect(Object.keys(draft[1]!).sort()).toEqual(["builtin", "position", "slug", "title", "widgets"]);
  draft[0]!.widgets[0]!.options!.search = "changed";
  expect(layout).toEqual(before);
});

it("renames without changing the slug and reorders, adds and removes with contiguous positions", () => {
  const pages = [page("a", 0), page("b", 1), page("c", 2)];
  expect(renamePage(pages, "b", "Work")?.map(p => [p.slug, p.title])).toEqual([["a", "A"], ["b", "Work"], ["c", "C"]]);
  expect(movePage(pages, "c", 0)?.map(p => [p.slug, p.position])).toEqual([["c", 0], ["a", 1], ["b", 2]]);
  expect(removePage(pages, "a")?.map(p => [p.slug, p.position])).toEqual([["b", 0], ["c", 1]]);
  expect(addPage(pages, "d", "D")?.map(p => p.position)).toEqual([0, 1, 2, 3]);
  expect(canonicalPages([page("x", 7), page("y", 2)]).map(p => p.position)).toEqual([0, 1]);
  expect(pages.map(p => p.position)).toEqual([0, 1, 2]);
});

it("refuses missing, duplicate, invalid and oversized changes without a partial change", () => {
  const pages = [page("a", 0, [widget("w1")]), page("b", 1, [widget("w2")])];
  expect(renamePage(pages, "z", "Z")).toBeUndefined();
  expect(movePage(pages, "a", 2)).toBeUndefined();
  expect(addPage(pages, "a", "Again")).toBeUndefined();
  expect(addPage(pages, "Bad slug", "Bad")).toBeUndefined();
  expect(addPage(pages, "c", " ")).toBeUndefined();
  expect(addPage(Array.from({ length: 30 }, (_, i) => page(`p${i}`, i)), "full", "Full")).toBeUndefined();
  expect(removePage(pages, "z")).toBeUndefined();
  expect(setPageWidgets(pages, "z", [])).toBeUndefined();
  expect(moveWidget(pages, "w9", "a", "b")).toBeUndefined();
  expect(moveWidget(pages, "w1", "a", "a")).toBeUndefined();
  expect(moveWidget([page("a", 0, [widget("w1")]), page("b", 1, [widget("w1", 0, { kind: "tests" })])], "w1", "a", "b")).toBeUndefined();
});

it("moves one widget instance to another page with its ID and options", () => {
  const pages = [page("a", 0, [widget("w1", 0, { options: { view: "done", search: "s" } }), widget("w3", 3)]), page("b", 1, [widget("w2", 0, { h: 5 })]), page("c", 2, [widget("w4")])];
  const moved = moveWidget(pages, "w1", "a", "b")!;
  expect(moved[0]!.widgets.map(w => w.id)).toEqual(["w3"]);
  expect(moved[1]!.widgets).toEqual([widget("w2", 0, { h: 5 }), widget("w1", 5, { options: { view: "done", search: "s" } })]);
  expect(moved[2]).toBe(pages[2]);
  expect(pages[0]!.widgets).toHaveLength(2);
});

function editor(api: { layout: () => Promise<ProjectLayout>; saveLayout: (project: string, body: ProjectLayoutInput) => Promise<ProjectLayout> }, project = () => "demo") {
  const listeners = new Set<(e: AgentEvent) => void>();
  const pages = { ...api, list: vi.fn(), get: vi.fn(), save: vi.fn(), remove: vi.fn(), resetLayout: vi.fn(), copyLayout: vi.fn() };
  return createRoot(dispose => {
    const value = createLayoutEditor({ pages }, { on: fn => { listeners.add(fn); return () => listeners.delete(fn); } }, "local", project);
    value.setAllowed(true);
    return { value, pages, emit: (data: Record<string, unknown>) => listeners.forEach(fn => fn({ seq: 1, id: "e", at: "", summary: "", kind: "page.updated", project: "demo", data })), dispose };
  });
}

it("saves all pages once with the start revision and canonical positions, then ends the draft", async () => {
  const saveLayout = vi.fn(async (_project: string, body: ProjectLayoutInput) => ({ revision: 5, pages: body.pages.map(p => ({ ...p, project: "demo", updated_at: "", updated_by: "operator" })) }));
  const { value, dispose } = editor({ layout: async () => snapshot(), saveLayout });
  try {
    await value.begin();
    expect(value.dirty()).toBe(false);
    value.update(p => movePage(p, "b", 0));
    value.update(p => renamePage(p, "a", "Renamed"));
    expect(value.dirty()).toBe(true);
    expect(await value.save()).toBe(true);
    expect(saveLayout).toHaveBeenCalledTimes(1);
    expect(saveLayout.mock.calls[0]![1]).toEqual({ base_revision: 4, pages: [
      { slug: "b", title: "B", position: 0, widgets: [widget("w2")], builtin: "board" },
      { slug: "a", title: "Renamed", position: 1, widgets: [widget("w1", 0, { options: { view: "done", search: "x" } })] },
    ] });
    expect(value.draft()).toBeUndefined();
    expect(value.committed()?.revision).toBe(5);
  } finally { dispose(); }
});

it("keeps the draft on page.updated and on 409, and replaces only on explicit request", async () => {
  const saveLayout = vi.fn(async (_project: string, body: ProjectLayoutInput): Promise<ProjectLayout> => {
    if (body.base_revision === 4) throw new ApiError(409, "conflict", undefined, "conflict", { revision: 6 });
    return { revision: 7, pages: [] };
  });
  const { value, emit, dispose } = editor({ layout: async () => snapshot(), saveLayout });
  try {
    await value.begin();
    value.update(p => renamePage(p, "a", "Mine"));
    emit({ project: "demo", revision: 6, operation: "replace" });
    expect(value.remote()).toBe(6);
    expect(value.page("a")?.title).toBe("Mine");
    expect(await value.save()).toBe(false);
    expect(value.conflict()).toBe(6);
    expect(value.page("a")?.title).toBe("Mine");
    expect(value.base()).toBe(4);
    expect(saveLayout).toHaveBeenCalledTimes(1);
    expect(await value.save(true)).toBe(true);
    expect(saveLayout.mock.calls[1]![1].base_revision).toBe(6);
    expect(value.draft()).toBeUndefined();
  } finally { dispose(); }
});

it("refuses edits and saves without rights, and drops a stale begin after Cancel", async () => {
  let resolve!: (layout: ProjectLayout) => void;
  const saveLayout = vi.fn();
  const { value, dispose } = editor({ layout: () => new Promise(r => { resolve = r; }), saveLayout });
  try {
    const started = value.begin();
    value.cancel();
    resolve(snapshot());
    await started;
    expect(value.draft()).toBeUndefined();
    value.setAllowed(false);
    await value.begin();
    expect(value.draft()).toBeUndefined();
    value.setAllowed(true);
    const again = value.begin(); resolve(snapshot()); await again;
    value.setAllowed(false);
    expect(value.editing()).toBe(false);
    expect(value.update(p => renamePage(p, "a", "No"))).toBe(false);
    expect(await value.save()).toBe(false);
    expect(saveLayout).not.toHaveBeenCalled();
  } finally { dispose(); }
});
