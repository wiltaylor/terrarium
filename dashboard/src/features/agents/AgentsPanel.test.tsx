// @vitest-environment jsdom
import { createSignal, type JSX } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import type { Agent, AgentEvent } from "../../api/types";
import { waitFor } from "../../test/wait";

const agent = (id: string, project = "demo", extra = {}): Agent => ({
  id, name: id, project, harness: "codex", status: "stopped", paused: false, roles: [], ...extra,
} as Agent);
const ordinary = [agent("paused", "demo", { paused: true }), agent("working", "demo", { status: "working" })];
const list = vi.fn(async (project: string) => [agent("ephemeral", project, { ephemeral: true })]);
const refresh = vi.fn(async () => {});
const navigate = vi.fn();
const listeners = new Set<(e: AgentEvent) => void>();
vi.mock("../../instance/navigation", () => ({ A: (p: JSX.AnchorHTMLAttributes<HTMLAnchorElement>) => <a {...p} />, useNavigate: () => navigate }));
vi.mock("../../components/Leases", () => ({ LeaseChips: () => null }));
vi.mock("../../components/HoldBadge", () => ({ HoldBadge: () => null }));
vi.mock("./NewAgentDialog", () => ({
  NewAgentDialog: (p: { project: string; onSaved: (a: Agent) => void }) => <button data-create={p.project} onClick={() => p.onSaved(agent("created"))}>Create fixture</button>,
}));
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({
    agents: { inProject: (p: string) => ordinary.filter(a => a.project === p), state: { loading: false, error: null }, refresh },
    api: { agents: { list } },
    events: { on: (fn: (e: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); } },
    leases: { loadAll: async () => {} },
    pins: { isPinned: () => false, toggle: async () => {} },
  });
});
const { AgentsPanel, CardAgentsPanel } = await import("./AgentsPanel");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); vi.clearAllMocks(); vi.useRealTimers(); });
function host() { const el = document.createElement("div"); document.body.append(el); return el; }

it("isolates duplicate status filters, includes paused, and creates in its project", () => {
  const el = host();
  dispose = render(() => <>
    <section><AgentsPanel project="demo" /></section>
    <section><AgentsPanel project="demo" /></section>
  </>, el);
  const sections = el.querySelectorAll("section");
  const select = sections[0]!.querySelector("select")!;
  select.value = "paused"; select.dispatchEvent(new Event("change", { bubbles: true }));
  expect(sections[0]!.querySelectorAll(".agent-card")).toHaveLength(1);
  expect(sections[0]!.textContent).toContain("paused");
  expect(sections[1]!.querySelectorAll(".agent-card")).toHaveLength(2);
  expect(list).not.toHaveBeenCalled();
  [...sections[0]!.querySelectorAll("button")].find(b => b.textContent === "New agent")!.click();
  const create = el.querySelector<HTMLButtonElement>("[data-create]")!;
  expect(create.dataset.create).toBe("demo");
  create.click();
  expect(navigate).toHaveBeenCalledWith("/agents/created");
});

it("owns separate card-agent requests and subscriptions, rejects stale replies and cleans debounce timers", async () => {
  let resolve!: (a: Agent[]) => void;
  list.mockImplementation(p => p === "old" ? new Promise(r => { resolve = r; }) : Promise.resolve([agent("fresh", p, { ephemeral: true }), agent("ordinary", p)]));
  const [project, setProject] = createSignal("old");
  const el = host();
  dispose = render(() => <>
    <section><CardAgentsPanel project={project()} /></section>
    <section><CardAgentsPanel project="demo" /></section>
  </>, el);
  await waitFor(() => expect(resolve).toBeTypeOf("function"));
  expect(list).toHaveBeenCalledWith("old", { card: true });
  expect(list).toHaveBeenCalledWith("demo", { card: true });
  setProject("new");
  await waitFor(() => expect(el.querySelector("section")!.textContent).toContain("fresh"));
  resolve([agent("stale", "old", { ephemeral: true })]);
  await Promise.resolve();
  expect(el.textContent).not.toContain("stale");
  expect(el.querySelectorAll(".agent-card")).toHaveLength(2);
  expect([...el.querySelectorAll("button")].some(b => b.textContent === "Pin")).toBe(false);
  expect(listeners.size).toBe(2);
  vi.useFakeTimers(); list.mockClear();
  listeners.forEach(fn => fn({ kind: "status", project: "wrong", data: {} } as AgentEvent));
  await vi.advanceTimersByTimeAsync(1100);
  expect(list).not.toHaveBeenCalled();
  listeners.forEach(fn => fn({ kind: "status", project: "demo", data: {} } as AgentEvent));
  await vi.advanceTimersByTimeAsync(1100);
  expect(list).toHaveBeenCalledExactlyOnceWith("demo", { card: true });
  list.mockClear();
  listeners.forEach(fn => fn({ kind: "card", project: "demo", data: {} } as AgentEvent));
  dispose(); dispose = undefined;
  await vi.advanceTimersByTimeAsync(1100);
  expect(list).not.toHaveBeenCalled();
  expect(listeners.size).toBe(0);
});
