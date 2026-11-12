// @vitest-environment jsdom
import { MemoryRouter, Route } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";
import { createMockApi } from "../../api/mock";
import type { AgentEvent } from "../../api/types";
const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const listeners = new Set<(e: AgentEvent) => void>();
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores({ api, projects: { prefixes: () => ({ TER: "terrarium" }) }, events: {
  on: (fn: (e: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); },
} }); });
const { WikiWidget } = await import("./WikiWidget");

it("resolves the current project's revision, refreshes it, and shows moved/deleted pages as missing", async () => {
  const page = await api.wiki.create("terrarium", { path: "widget-test", title: "Local guide", body: "# Local content", summary: "initial" });
  const get = vi.spyOn(api.wiki, "get");
  const host = document.createElement("div"); document.body.append(host);
  const dispose = render(() => createComponent(MemoryRouter, { get children() { return createComponent(Route, { path: "*", component: () => createComponent(WikiWidget, { project: "terrarium", path: "widget-test" }) }); } }), host);
  const event = (kind: AgentEvent["kind"]) => listeners.forEach(fn => fn({ seq: 1, id: "wiki-event", at: "", kind, project: "terrarium", summary: "wiki change", data: {} }));
  try {
    await waitFor(() => expect(host.textContent).toContain("Local content"));
    expect(get).toHaveBeenCalledWith("terrarium/widget-test");
    expect(host.querySelector('a')?.getAttribute("href")).toContain("/p/terrarium/knowledge/wiki/widget-test");
    await api.wiki.save(page.id, { body: "# New revision", base: page.rev, summary: "update" }); event("wiki.saved");
    await waitFor(() => expect(host.textContent).toContain("New revision"));
    expect(host.textContent).toContain("Revision 2");
    await api.wiki.move(page.id, "moved-widget"); event("wiki.moved");
    await waitFor(() => expect(host.textContent).toContain("page not found"));
    expect(host.textContent).not.toContain("New revision");
    await api.wiki.remove(page.id); event("wiki.deleted");
    await waitFor(() => expect(host.textContent).toContain("page not found"));
  } finally { dispose(); host.remove(); vi.restoreAllMocks(); }
});

it("does not render a page returned from another project", async () => {
  const page = await api.wiki.get("terrarium/index");
  vi.spyOn(api.wiki, "get").mockResolvedValue({ ...page, project: "other", body: "Foreign content" });
  const host = document.createElement("div"); document.body.append(host);
  const dispose = render(() => createComponent(MemoryRouter, { get children() { return createComponent(Route, { path: "*", component: () => createComponent(WikiWidget, { project: "terrarium", path: "index" }) }); } }), host);
  try { await waitFor(() => expect(host.textContent).toContain("page not found")); expect(host.textContent).not.toContain("Foreign content"); }
  finally { dispose(); host.remove(); vi.restoreAllMocks(); }
});
