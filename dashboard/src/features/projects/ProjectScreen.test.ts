// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";
import { PROJECT_TABS } from "../../app/nav";
import type { Agent, AgentEvent, ProjectCounts } from "../../api/types";

// The stores of the tab counts (TER-154): no badge unless a test sets one.
const state = vi.hoisted(() => ({
  agents: [] as Pick<Agent, "project" | "status">[],
  counts: { blocked_cards: 0, questions: { open: 0, urgent: 0, critical: 0 } } as ProjectCounts,
  listeners: [] as ((e: AgentEvent) => void)[],
  reads: [] as string[],
  dashboard: false,
}));
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores(({
  agents: { inProject: (p: string) => state.agents.filter((a) => a.project === p) },
  api: {
    pages: { list: async () => ({ items: state.dashboard ? [{ slug: "dashboard", title: "Dashboard", position: 0, widgets: [] }] : [], can_edit: false, dashboard_slug: "dashboard" }) },
    projects: {
      counts: async (p: string) => {
        state.reads.push(p);
        return structuredClone(state.counts);
      },
    },
  },
  events: {
    on: (fn: (e: AgentEvent) => void) => {
      state.listeners.push(fn);
      return () => state.listeners.splice(state.listeners.indexOf(fn), 1);
    },
  },
})); });

const { default: ProjectScreen } = await import("./ProjectScreen");

let dispose: (() => void) | undefined;
afterEach(() => {
  state.dashboard = false;
  state.agents = [];
  state.counts = { blocked_cards: 0, questions: { open: 0, urgent: 0, critical: 0 } };
  state.reads = [];
  dispose?.();
  dispose = undefined;
  document.body.replaceChildren();
  vi.unstubAllGlobals();
  Reflect.deleteProperty(Element.prototype, "scrollIntoView");
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
          return createComponent(Route, { path: "/p/:project/*", component: ProjectScreen });
        },
      }),
    root,
  );
  return { root, history };
}

function links(root: HTMLElement) {
  return Array.from(root.querySelectorAll<HTMLAnchorElement>('nav[aria-label="Project"] a'));
}

function active(root: HTMLElement) {
  return links(root).filter((a) => a.classList.contains("active"));
}

describe("ProjectScreen", () => {
  it("shows the project title and every tab in order, with Settings last", () => {
    const { root } = mount("/p/my%20app/board");
    expect(root.querySelector("h1")?.textContent).toBe("my app");
    expect(links(root).map((a) => a.textContent)).toEqual(PROJECT_TABS.map((t) => t.label));
    expect(links(root).map((a) => a.getAttribute("href"))).toEqual(PROJECT_TABS.map((t) => `/p/my%20app/${t.path}`));
    expect(links(root).every((a) => a.tabIndex === 0)).toBe(true);
  });

  it("selects the tab from the first segment on deep paths", () => {
    for (const [path, label] of [
      ["/p/demo/board/artifacts/ART-2", "Board"],
      ["/p/demo/tests/1", "Tests"],
      ["/p/demo/knowledge/wiki/a/b", "Knowledge"],
      ["/p/demo/repos/infra", "Repos"],
      ["/p/demo/settings/secrets", "Settings"],
    ] as const) {
      const { root } = mount(path);
      expect(active(root).map((a) => a.textContent), path).toEqual([label]);
      dispose?.();
      document.body.replaceChildren();
    }
  });

  it("moves between tabs with browser back and forward", async () => {
    state.dashboard = true;
    const { root, history } = mount("/p/demo/tests/1");
    await waitFor(() => expect(links(root).some(a => a.textContent === "Dashboard")).toBe(true));
    links(root).find((a) => a.textContent === "Dashboard")!.click();
    await waitFor(() => expect(active(root).map((a) => a.textContent)).toEqual(["Dashboard"]));
    links(root).find((a) => a.textContent === "Knowledge")!.click();
    await waitFor(() => expect(active(root).map((a) => a.textContent)).toEqual(["Knowledge"]));
    history.back();
    await waitFor(() => expect(active(root).map((a) => a.textContent)).toEqual(["Dashboard"]));
    history.forward();
    await waitFor(() => expect(active(root).map((a) => a.textContent)).toEqual(["Knowledge"]));
  });

  it("reveals the active tab horizontally, including after navigation, without scrolling the document", async () => {
    vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
      if (this.tagName === "NAV") return { left: 0, right: 300 } as DOMRect;
      const scroll = this.closest("nav")?.scrollLeft ?? 0;
      const left = (this.textContent === "Settings" ? 400 : 0) - scroll;
      return { left, right: left + 100 } as DOMRect;
    });
    const { root } = mount("/p/demo/settings/secrets");
    const nav = root.querySelector("nav")!;
    expect(nav.scrollLeft).toBe(200);
    links(root).find((a) => a.textContent === "Board")!.click();
    await waitFor(() => expect(nav.scrollLeft).toBe(0));
    vi.restoreAllMocks();
  });
  it("shows the blocked agents, blocked cards, and open questions on their tabs, in the accessible name", async () => {
    state.dashboard = true;
    state.agents = [
      { project: "demo", status: "blocked" },
      { project: "demo", status: "error" },
      { project: "demo", status: "idle" },
      { project: "other", status: "blocked" },
    ];
    state.counts = { blocked_cards: 1, questions: { open: 3, urgent: 1, critical: 0 } };
    const { root } = mount("/p/demo/board");
    const tab = (label: string) => links(root).find((a) => a.textContent?.startsWith(label))!;
    await waitFor(() => expect(tab("Board").textContent).toBe("Board1, 1 blocked card"));
    expect(state.reads).toEqual(["demo"]);
    expect(tab("Agents").textContent).toBe("Agents2, 2 agents blocked or in error");
    expect(tab("Dashboard").textContent).toBe("Dashboard3, 3 open questions, 1 urgent");
    expect(tab("Dashboard").title).toBe("3 open questions, 1 urgent");
    // The number is hidden from a screen reader: the words say it once.
    expect(tab("Board").querySelector(".nav-count")?.getAttribute("aria-hidden")).toBe("true");
    expect(tab("Board").querySelector(".sr-only")?.textContent).toBe(", 1 blocked card");
    expect(tab("Board").querySelector(".nav-count")?.className).toBe("nav-count warn");
    expect(tab("Tests").textContent).toBe("Tests");
  });

  it("reads the counts again after a card event of the project, and not for another project", async () => {
    state.dashboard = true;
    vi.useFakeTimers();
    try {
      const { root } = mount("/p/demo/board");
      await vi.advanceTimersByTimeAsync(0);
      expect(state.reads).toEqual(["demo"]);
      const board = () => links(root).find((a) => a.textContent?.startsWith("Board"))!;
      expect(board().textContent).toBe("Board");
      state.counts = { blocked_cards: 2, questions: null };
      const send = (e: Partial<AgentEvent>) => state.listeners.forEach((fn) => fn(e as AgentEvent));
      send({ kind: "card", project: "other" });
      send({ kind: "status", project: "demo" });
      await vi.advanceTimersByTimeAsync(400);
      expect(state.reads).toEqual(["demo"]);
      send({ kind: "card", project: "demo" });
      send({ kind: "card", project: "demo" });
      await vi.advanceTimersByTimeAsync(400);
      expect(state.reads).toEqual(["demo", "demo"]);
      expect(board().textContent).toBe("Board2, 2 blocked cards");
      // `questions: null` (a reader who may not list them): no question count.
      expect(links(root).find((a) => a.textContent?.startsWith("Dashboard"))!.textContent).toBe("Dashboard");
    } finally {
      vi.useRealTimers();
    }
  });
});
