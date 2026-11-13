// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";
import { createMockApi } from "../../api/mock";
import type { TabBadge } from "./tabCounts";
import type { AgentEvent } from "../../api/types";
const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const listeners = new Set<(e: AgentEvent) => void>();
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores({ api, events: {
  on: (fn: (e: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); },
} }); });
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
it("retains widget badges and the dashboard alias policy during a page refresh", async () => {
  const response = { items: [{ project: "terrarium", slug: "dashboard", title: "Work", position: 0, updated_at: "", updated_by: "operator", widgets: [
    { id: "board", kind: "board" as const, x: 0, y: 0, w: 12, h: 3 },
  ] }], can_edit: false, capabilities: { project_pages_v2: true } };
  let resolve!: (value: typeof response) => void;
  const list = vi.spyOn(api.pages, "list").mockResolvedValueOnce(response).mockImplementationOnce(() => new Promise(r => { resolve = r; }));
  const { host, dispose } = mount(path => path === "board" ? { n: 3, cls: "nav-count warn", text: "3 blocked cards" } : path === "dashboard" ? { n: 2, cls: "nav-count info", text: "2 open questions" } : null);
  const tab = () => host.querySelector('a[href$="/pages/dashboard"]');
  try {
    await waitFor(() => expect(tab()?.textContent).toContain("3 blocked cards"));
    expect(tab()?.textContent).not.toContain("2 open questions");
    listeners.forEach(fn => fn({ seq: 1, id: "page-event", at: new Date().toISOString(), summary: "Page updated", kind: "page.updated", project: "terrarium", data: { slug: "dashboard" } }));
    await waitFor(() => expect(list).toHaveBeenCalledTimes(2));
    expect(tab()?.textContent).toContain("3 blocked cards");
    expect(tab()?.textContent).not.toContain("2 open questions");
    resolve(response);
    await waitFor(() => expect(tab()?.textContent).toContain("3 blocked cards"));
  } finally {
    resolve?.(response);
    dispose();
  }
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
it("does not infer a question-bearing Dashboard from its slug when no dashboard is designated", async () => {
  const page = { project: "terrarium", slug: "dashboard", title: "Old dashboard slug", position: 0, updated_at: "", updated_by: "operator", widgets: [] };
  vi.spyOn(api.pages, "list").mockResolvedValue({ items: [page], can_edit: false, capabilities: { project_pages_v2: true } });
  const { host, dispose } = mount(path => path === "dashboard" ? { n: 2, cls: "nav-count info", text: "2 open questions" } : null);
  try {
    await waitFor(() => expect(host.textContent).toContain("Old dashboard slug"));
    expect(host.querySelector('a[href$="/pages/dashboard"] .nav-count')).toBeNull();
  } finally { dispose(); }
});
