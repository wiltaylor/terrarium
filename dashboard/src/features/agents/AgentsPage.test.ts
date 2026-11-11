// @vitest-environment jsdom
// The Agents tab does what the sidebar agent rows did (TER-153): a pin per
// agent, and New agent. The agent page shows the project tabs with Agents active.
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent, createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";
import { PROJECT_TABS } from "../../app/nav";

const [pinned, setPinned] = createSignal<string[]>([]);
const toggle = vi.fn(async (id: string) => {
  setPinned(pinned().includes(id) ? pinned().filter((p) => p !== id) : [...pinned(), id]);
});
const list = [
  { id: "A1", name: "lead", project: "my app", harness: "claude", status: "idle", roles: [], paused: false },
  { id: "A2", name: "worker", project: "my app", harness: "codex", status: "working", roles: [], paused: false },
];

vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores(({
  agents: { inProject: (p: string) => list.filter((a) => a.project === p), byId: () => undefined, refresh: async () => {}, state: { error: null, loading: false } },
  api: { agents: { list: async () => [{ ...list[0], id: "E1", name: "ter-1", ephemeral: true }] } },
  events: { on: () => () => {} },
  leases: { loadAll: async () => {} },
  pins: { isPinned: (id: string) => pinned().includes(id), toggle },
})); });
vi.mock("../../components/Leases", () => ({ LeaseChips: () => null }));
vi.mock("../../components/HoldBadge", () => ({ HoldBadge: () => null }));
vi.mock("./NewAgentDialog", () => ({
  NewAgentDialog: (p: { project: string }) => {
    const d = document.createElement("div");
    d.setAttribute("role", "dialog");
    d.textContent = `new agent in ${p.project}`;
    return d;
  },
}));

const { AgentsPage } = await import("./AgentsPage");
const { ProjectTabs } = await import("../projects/ProjectTabs");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  setPinned([]);
  toggle.mockClear();
  document.body.replaceChildren();
});

function mount(path: string, pattern: string, component: () => unknown) {
  const root = document.createElement("div");
  document.body.append(root);
  const history = createMemoryHistory();
  history.set({ value: path });
  dispose = render(
    () =>
      createComponent(MemoryRouter, {
        history,
        get children() {
          return createComponent(Route, { path: pattern, component: component as never });
        },
      }),
    root,
  );
  return root;
}

const card = (root: HTMLElement, name: string) => Array.from(root.querySelectorAll<HTMLElement>(".agent-card")).find((c) => c.querySelector("a.name")?.textContent === name)!;

describe("AgentsPage", () => {
  it("pins and unpins an agent from its card", async () => {
    const root = mount("/p/my%20app/agents", "/p/:project/agents", AgentsPage);
    await waitFor(() => expect(card(root, "worker")).toBeTruthy());
    const pin = Array.from(card(root, "worker").querySelectorAll("button")).find((b) => b.textContent === "Pin")!;
    expect(pin.getAttribute("aria-pressed")).toBe("false");
    pin.click();
    expect(toggle).toHaveBeenCalledWith("A2");
    await waitFor(() => expect(pin.textContent).toBe("Unpin"));
    expect(pin.getAttribute("aria-pressed")).toBe("true");
    pin.click();
    await waitFor(() => expect(pin.textContent).toBe("Pin"));
  });

  it("gives a card agent no pin", async () => {
    const root = mount("/p/my%20app/agents", "/p/:project/agents", AgentsPage);
    await waitFor(() => expect(root.querySelector(".card-agents .agent-card")).toBeTruthy());
    const buttons = Array.from(root.querySelectorAll(".card-agents button")).map((b) => b.textContent);
    expect(buttons).not.toContain("Pin");
  });

  it("opens New agent for the route project", async () => {
    const root = mount("/p/my%20app/agents", "/p/:project/agents", AgentsPage);
    Array.from(root.querySelectorAll("button")).find((b) => b.textContent === "New agent")!.click();
    await waitFor(() => expect(root.querySelector('[role="dialog"]')?.textContent).toBe("new agent in my app"));
  });
});

describe("ProjectTabs on the agent page", () => {
  it("shows every project tab with Agents active, outside /p", () => {
    const root = mount("/agents/A1", "/agents/:id", () => ProjectTabs({ base: "/p/my%20app", active: (t) => t === "agents" }));
    const links = Array.from(root.querySelectorAll<HTMLAnchorElement>('nav[aria-label="Project"] a'));
    expect(links.map((a) => a.getAttribute("href"))).toEqual(PROJECT_TABS.map((t) => `/p/my%20app/${t.path}`));
    expect(links.filter((a) => a.classList.contains("active")).map((a) => a.textContent)).toEqual(["Agents"]);
  });
});
