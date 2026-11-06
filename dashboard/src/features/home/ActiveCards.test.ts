// @vitest-environment jsdom
// Home, Work in progress (TER-43): the section lists the cards by project
// with links, the column, the agent and its status, and the ETA; an empty
// list shows the empty state.
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ActiveCard } from "../../api/types";

const active = vi.fn();
vi.mock("../../stores", () => ({
  api: { cards: { active } },
  events: { on: () => () => {} },
}));
vi.mock("@solidjs/router", () => ({
  A: (p: { href: string; children?: unknown }) => {
    const a = document.createElement("a");
    a.setAttribute("href", p.href);
    a.textContent = String(p.children ?? "");
    return a;
  },
}));

const { ActiveCards } = await import("./ActiveCards");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  vi.clearAllMocks();
});

const card = (over: Partial<ActiveCard>): ActiveCard =>
  ({
    id: "TER-1",
    project: "terrarium",
    column: "in_progress",
    column_kind: "working",
    position: 0,
    title: "Parser",
    body: "",
    labels: [],
    depends_on: [],
    blocked_by: [],
    comments: [],
    links: [],
    created_by: "operator",
    created_by_name: "operator",
    created_at: "",
    updated_at: "",
    archived: false,
    ...over,
  }) as ActiveCard;

async function mount(now: number) {
  const el = document.createElement("div");
  document.body.append(el);
  dispose = render(() => createComponent(ActiveCards, { now }), el);
  await vi.waitFor(() => expect(active).toHaveBeenCalled());
  await new Promise((r) => setTimeout(r, 0));
  return el;
}

describe("ActiveCards", () => {
  it("lists the cards by project with the agent, its status, and the ETA", async () => {
    active.mockResolvedValue([
      card({ id: "AB-2", project: "abc", title: "Spike", column: "research", column_kind: "research" }),
      card({ assignee: "A1", assignee_name: "terrarium/rust-ci", assignee_status: "working", eta_at: "2026-11-05T11:00:00Z" }),
    ]);
    const el = await mount(Date.parse("2026-11-05T10:00:00Z"));
    await vi.waitFor(() => expect(el.querySelectorAll("tr[data-card]").length).toBe(2));
    const groups = [...el.querySelectorAll(".active-project h3")].map((h) => h.textContent);
    expect(groups).toEqual(["abc", "terrarium"]);
    const row = el.querySelector('tr[data-card="TER-1"]')!;
    expect(row.textContent).toContain("Parser");
    expect(row.textContent).toContain("in progress");
    expect(row.textContent).toContain("terrarium/rust-ci");
    expect(row.textContent).toContain("working");
    expect(row.textContent).toContain("1h 00m left");
    const links = [...row.querySelectorAll("a")].map((a) => a.getAttribute("href"));
    expect(links).toContain("/p/terrarium/board?card=TER-1");
    expect(links).toContain("/agents/A1");
    const spike = el.querySelector('tr[data-card="AB-2"]')!;
    expect(spike.textContent).toContain("research");
    expect(spike.textContent).toContain("unassigned");
    expect(spike.textContent).toContain("no ETA");
    expect(el.textContent).toContain("1 in progress, 1 in research");
  });

  it("lines up the columns: one table and one column template for all projects, the same cells in each row", async () => {
    active.mockResolvedValue([
      card({ id: "AB-2", project: "abc", title: "Spike", column: "research", column_kind: "research" }),
      card({ id: "AB-3", project: "abc", title: "A very long title that the column cuts with an ellipsis", assignee: "A2", assignee_name: "abc/dev" }),
      card({ assignee: "A1", assignee_name: "terrarium/rust-ci", assignee_status: "working", eta_at: "2026-11-05T11:00:00Z" }),
    ]);
    const el = await mount(Date.parse("2026-11-05T10:00:00Z"));
    await vi.waitFor(() => expect(el.querySelectorAll("tr[data-card]").length).toBe(3));

    // One table, one colgroup of five columns: every group shares the widths.
    const tables = el.querySelectorAll("table");
    expect(tables.length).toBe(1);
    expect(tables[0]!.classList.contains("active-table")).toBe(true);
    const cols = [...tables[0]!.querySelectorAll(":scope > colgroup > col")].map((c) => c.className);
    expect(cols).toEqual(["col-id", "col-title", "col-column", "col-agent", "col-eta"]);
    expect([...tables[0]!.querySelectorAll("thead th")].map((t) => t.textContent)).toEqual(["Card", "Title", "Column", "Agent", "ETA"]);

    // A group head row per project, across all columns.
    const heads = [...el.querySelectorAll("tbody.active-project > tr.active-group > th")];
    expect(heads.map((h) => h.textContent)).toEqual(["abc", "terrarium"]);
    for (const h of heads) expect(h.getAttribute("colspan")).toBe("5");

    // Every card row has the same five cells, in the same order.
    const shape = (tr: Element) =>
      [...tr.children].map((td) => {
        if (td.querySelector('a[href*="?card="]')) return "id";
        if (td.classList.contains("active-title")) return "title";
        if (td.querySelector(".chip") && !td.classList.contains("active-agent")) return "column";
        if (td.classList.contains("active-agent")) return "agent";
        return "eta";
      });
    for (const tr of el.querySelectorAll("tr[data-card]")) {
      expect(tr.children.length).toBe(5);
      expect(shape(tr)).toEqual(["id", "title", "column", "agent", "eta"]);
    }

    // The full title is the tooltip of the cut cell.
    const long = el.querySelector('tr[data-card="AB-3"] td.active-title')!;
    expect(long.getAttribute("title")).toBe("A very long title that the column cuts with an ellipsis");
  });

  it("shows the idle mark of a card next to the agent status (TER-67)", async () => {
    active.mockResolvedValue([
      card({ assignee: "A1", assignee_name: "terrarium/w2", assignee_status: "idle", idle_since: "2026-11-05T09:40:00.000Z" }),
      card({ id: "TER-2", assignee: "A2", assignee_name: "terrarium/w3", assignee_status: "working" }),
    ]);
    const el = await mount(Date.parse("2026-11-05T10:00:00Z"));
    await vi.waitFor(() => expect(el.querySelectorAll("tr[data-card]").length).toBe(2));
    const badge = el.querySelector('tr[data-card="TER-1"] td.active-agent .idle-badge')!;
    expect(badge.textContent).toBe("idle 20m");
    expect(badge.getAttribute("title")).toContain("leads were told");
    expect(el.querySelector('tr[data-card="TER-2"] .idle-badge')).toBeNull();
  });

  it("shows the empty state", async () => {
    active.mockResolvedValue([]);
    const el = await mount(Date.now());
    await vi.waitFor(() => expect(el.textContent).toContain("No card is in progress or in research."));
  });
});
