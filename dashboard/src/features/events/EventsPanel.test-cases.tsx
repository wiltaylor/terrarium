// @vitest-environment jsdom
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import type { AgentEvent, EventQuery } from "../../api/types";
import { waitFor } from "../../test/wait";

const listeners = new Set<(event: AgentEvent) => void>();
const event = (project: string, seq: number): AgentEvent => ({
  project, seq, id: `${project}-${seq}`, kind: "mail", at: "2026-11-13T00:00:00Z", summary: `${project} event ${seq}`, data: {},
});
const history = vi.fn(async (q: EventQuery) => ({ items: [event(q.project!, 1)], next: null }));
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({
    api: { events: { history } },
    projects: { selected: () => "wrong-project" },
    agents: { inProject: (p: string) => [{ id: `${p}-agent`, project: p, name: "lead" }] },
    events: {
      status: () => ({ lastSeq: 1 }), state: () => "live",
      on: (fn: (event: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); },
    },
  });
});
const { EventsPanel } = await import("./EventsPage");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); dispose = undefined; document.body.replaceChildren(); document.documentElement.scrollTop = 0; vi.restoreAllMocks(); history.mockClear(); });

it("keeps duplicate filters local and never consults the global project", async () => {
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <><section><EventsPanel project="alpha" /></section><section><EventsPanel project="beta" /></section></>, root);
  await waitFor(() => expect(root.querySelectorAll(".event-row")).toHaveLength(2));
  expect(root.querySelector('[aria-label="Scope"]')).toBeNull();
  const kinds = root.querySelectorAll<HTMLSelectElement>('[aria-label="Kind"]');
  kinds[0]!.value = "mail"; kinds[0]!.dispatchEvent(new Event("change", { bubbles: true }));
  await waitFor(() => expect(history).toHaveBeenCalledWith(expect.objectContaining({ project: "alpha", kind: "mail" })));
  expect(kinds[1]!.value).toBe("");
  expect(history.mock.calls.every(([q]) => q.project === "alpha" || q.project === "beta")).toBe(true);
  expect(root.querySelectorAll('[aria-label="Agent"]')[0]!.textContent).not.toContain("beta");
});

it("retains rows, focus and filter state on live arrival and cleans subscriptions", async () => {
  history.mockResolvedValueOnce({ items: [event("alpha", 2), event("alpha", 1)], next: null });
  // jsdom has no layout. Model row geometry on the real document scroll owner;
  // the production keeper must compensate when a live row is inserted above it.
  const nativeRect = Element.prototype.getBoundingClientRect;
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
    if (this.matches(".event-row, .event-list")) {
      const index = this.matches(".event-row") ? [...this.parentElement!.children].indexOf(this) : 0;
      return new DOMRect(0, index * 40 - document.documentElement.scrollTop, 500, 40);
    }
    return nativeRect.call(this);
  });
  const scrollBy = vi.spyOn(window, "scrollBy").mockImplementation((options, y) => {
    document.documentElement.scrollTop += typeof options === "number" ? y ?? 0 : options?.top ?? 0;
  });
  const [project, setProject] = createSignal("alpha");
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <EventsPanel project={project()} />, root);
  await waitFor(() => expect(root.querySelector(".event-row")).toBeTruthy());
  const row = root.querySelectorAll(".event-row")[1]!;
  const since = root.querySelector<HTMLInputElement>('[aria-label="Since"]')!;
  since.focus();
  document.documentElement.scrollTop = 50;
  window.dispatchEvent(new Event("scroll"));
  const top = row.getBoundingClientRect().top;
  for (const fn of listeners) fn(event("beta", 3));
  expect(root.querySelectorAll(".event-row")).toHaveLength(2);
  for (const fn of listeners) fn(event("alpha", 3));
  await waitFor(() => expect(root.querySelectorAll(".event-row")).toHaveLength(3));
  await waitFor(() => expect(scrollBy).toHaveBeenCalledWith(0, 40));
  expect(root.querySelectorAll(".event-row")[2]).toBe(row);
  expect(document.activeElement).toBe(since);
  expect(document.documentElement.scrollTop).toBe(90);
  expect(row.getBoundingClientRect().top).toBe(top);
  expect(listeners.size).toBe(1);
  setProject("beta");
  await waitFor(() => expect(root.textContent).toContain("beta event 1"));
  expect(root.textContent).not.toContain("alpha event");
  expect(listeners.size).toBe(1);
  dispose!(); dispose = undefined;
  expect(listeners.size).toBe(0);
});

it("does not fetch an empty project as all projects", () => {
  const root = document.createElement("div");
  dispose = render(() => <EventsPanel project="" />, root);
  expect(history).not.toHaveBeenCalled();
  expect(listeners.size).toBe(0);
});
