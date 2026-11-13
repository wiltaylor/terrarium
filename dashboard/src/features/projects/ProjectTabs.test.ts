// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";
import { createMockApi } from "../../api/mock";
import type { TabBadge } from "./tabCounts";
const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores({ api, events: { on: () => () => {} } }); });
const { ProjectTabs } = await import("./ProjectTabs");
function mount(badge?: (path: string) => TabBadge | null) {
  const history = createMemoryHistory(); history.set({ value: "/p/terrarium/board" });
  const host = document.createElement("div"); document.body.append(host);
  const dispose = render(() => createComponent(MemoryRouter, { history, get children() { return createComponent(Route, { path: "*", component: () => createComponent(ProjectTabs, { base: "/p/terrarium", active: p => p === "board", badge }) }); } }), host);
  return { host, dispose: () => { dispose(); host.remove(); vi.restoreAllMocks(); } };
}
it("places accessible counts on pages by widget location only when enabled", async () => {
  const page = { project: "terrarium", slug: "work", title: "Work", position: 0, updated_at: "", updated_by: "operator", widgets: [
    { id: "one", kind: "board" as const, x: 0, y: 0, w: 12, h: 3 },
    { id: "two", kind: "board" as const, x: 0, y: 3, w: 12, h: 3 },
  ] };
  vi.spyOn(api.pages, "list").mockResolvedValue({ items: [page], can_edit: false, capabilities: { project_pages_v2: true } });
  const { host, dispose } = mount(path => path === "board" ? { n: 3, cls: "nav-count warn", text: "3 blocked cards" } : null);
  try {
    await waitFor(() => expect(host.querySelector('a[href$="/pages/work"]')?.textContent).toContain("3 blocked cards"));
    expect(host.querySelector('a[href$="/pages/work"] .nav-count')?.textContent).toBe("3");
  } finally { dispose(); }
});
it("puts ordered custom tabs before Board and Add page at the end, and creates a page", async () => {
  await api.pages.save("terrarium", "later", { title: "Later", position: 2, widgets: [] });
  await api.pages.save("terrarium", "first", { title: "First", position: 0, widgets: [] });
  const { host, dispose } = mount();
  try {
    await waitFor(() => expect(host.textContent).toContain("First"));
    expect([...host.querySelectorAll("a")].slice(0, 3).map(a => a.textContent)).toEqual(["First", "Later", "Board"]);
    expect(host.querySelector("nav")?.lastElementChild?.textContent).toBe("Add page");
    host.querySelector("button")!.click();
    const inputs = host.querySelectorAll("input");
    inputs[0]!.value = "new-page"; inputs[0]!.dispatchEvent(new Event("input", { bubbles: true }));
    inputs[1]!.value = "New page"; inputs[1]!.dispatchEvent(new Event("input", { bubbles: true }));
    host.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await waitFor(() => expect(host.textContent).toContain("New page"));
    expect((await api.pages.get("terrarium", "new-page")).position).toBe(3);
  } finally { dispose(); }
});
it("does not expose Add page to a reader", async () => {
  vi.spyOn(api.pages, "list").mockResolvedValue({ items: [], can_edit: false });
  const { host, dispose } = mount();
  try { await waitFor(() => expect(host.textContent).toContain("Board")); expect(host.textContent).not.toContain("Add page"); }
  finally { dispose(); }
});
