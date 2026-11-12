// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";
import { createMockApi } from "../../api/mock";
import type { AgentEvent } from "../../api/types";
import { ApiError } from "../../api/http";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const listeners = new Set<(e: AgentEvent) => void>();
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores({ api, events: {
  on: (fn: (e: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); },
} }); });
const { default: CustomPage } = await import("./CustomPage");

async function mount() {
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  await api.pages.save("terrarium", "overview", { title: "Overview", position: 0, widgets: [] });
  const history = createMemoryHistory(); history.set({ value: "/p/terrarium/pages/overview" });
  const host = document.createElement("div"); document.body.append(host);
  const dispose = render(() => createComponent(MemoryRouter, { history, get children() { return createComponent(Route, { path: "/p/:project/pages/:slug", component: CustomPage }); } }), host);
  await waitFor(() => expect(host.textContent).toContain("Overview"));
  return { host, dispose: () => { dispose(); host.remove(); vi.restoreAllMocks(); vi.unstubAllGlobals(); } };
}
function click(host: HTMLElement, text: string) {
  const button = [...host.querySelectorAll("button")].find(b => b.textContent === text);
  expect(button).toBeTruthy(); button!.click();
}

it("keeps an edit draft when another editor emits page.updated", async () => {
  const { host, dispose } = await mount();
  try {
    await waitFor(() => expect(host.textContent).toContain("Edit page"));
    click(host, "Edit page");
    const input = host.querySelector("input")!; input.value = "My draft"; input.dispatchEvent(new Event("input", { bubbles: true }));
    await api.pages.save("terrarium", "overview", { title: "Other editor", position: 0, widgets: [] });
    listeners.forEach(fn => fn({ kind: "page.updated", project: "terrarium", data: { slug: "overview" } } as AgentEvent));
    await waitFor(() => expect(host.querySelector("input")?.value).toBe("My draft"));
    click(host, "Save page");
    await waitFor(() => expect(host.textContent).toContain("My draft"));
    expect((await api.pages.get("terrarium", "overview")).title).toBe("My draft");
  } finally { dispose(); }
});

it("hides write controls for a reader", async () => {
  vi.spyOn(api.pages, "list").mockResolvedValue({ items: [], can_edit: false });
  const { host, dispose } = await mount();
  try { await waitFor(() => expect(host.textContent).toContain("This page has no widgets")); expect(host.textContent).not.toContain("Edit page"); }
  finally { dispose(); }
});

it("shows daemon validation refusal and retains the draft", async () => {
  const { host, dispose } = await mount();
  try {
    await waitFor(() => expect(host.textContent).toContain("Edit page")); click(host, "Edit page");
    vi.spyOn(api.pages, "save").mockRejectedValue(new ApiError(422, "widget exceeds 10000 rows"));
    click(host, "Save page");
    await waitFor(() => expect(host.querySelector('[role="alert"]')?.textContent).toContain("10000 rows"));
    expect(host.querySelector("input")?.value).toBe("Overview");
  } finally { dispose(); }
});

it("keeps the loaded page visible while a live refresh is pending", async () => {
  const { host, dispose } = await mount();
  try {
    const get = vi.spyOn(api.pages, "get");
    let resolve!: (value: Awaited<ReturnType<typeof api.pages.get>>) => void;
    const previous = await api.pages.get("terrarium", "overview");
    get.mockImplementation(() => new Promise(r => { resolve = r; }));
    listeners.forEach(fn => fn({ kind: "page.updated", project: "terrarium", data: { slug: "overview" } } as AgentEvent));
    expect(host.textContent).toContain("Overview");
    expect(host.textContent).toContain("This page has no widgets");
    resolve(previous);
    await waitFor(() => expect(host.textContent).toContain("Overview"));
  } finally { dispose(); }
});
