// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";
import type { AgentEvent } from "../../api/types";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const listeners = new Set<(e: AgentEvent) => void>();
vi.mock("../../stores", () => ({ api, events: {
  on: (fn: (e: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); },
} }));
const { default: DashboardPage } = await import("./DashboardPage");

it("loads accuracy once and does not reload it with monitor results", async () => {
  const accuracy = vi.spyOn(api.monitors, "etaAccuracy");
  const dashboard = vi.spyOn(api.monitors, "dashboard");
  const history = createMemoryHistory();
  history.set({ value: "/p/terrarium/dashboard" });
  const host = document.createElement("div");
  document.body.append(host);
  const dispose = render(() => createComponent(MemoryRouter, {
    history,
    get children() { return createComponent(Route, { path: "/p/:project/dashboard", component: DashboardPage }); },
  }), host);
  try {
    await vi.waitFor(() => expect(host.textContent).toContain("ETA accuracy"));
    expect(accuracy).toHaveBeenCalledExactlyOnceWith("terrarium");
    listeners.forEach(fn => fn({ kind: "monitor.result", project: "terrarium", data: {} } as AgentEvent));
    await vi.waitFor(() => expect(dashboard).toHaveBeenCalledTimes(2));
    expect(accuracy).toHaveBeenCalledTimes(1);
  } finally {
    dispose(); host.remove(); vi.restoreAllMocks();
  }
});
