// @vitest-environment jsdom
// Home, Blocked (TER-51): the section asks for the blocked column, lists the
// cards by project in one table with the same cells in each row (link,
// title, the first line of the reason, the agent and its status, how long),
// and shows the empty state.
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

const { BlockedCards } = await import("./BlockedCards");

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
    column: "blocked",
    column_kind: "blocked",
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
  dispose = render(() => createComponent(BlockedCards, { now }), el);
  await vi.waitFor(() => expect(active).toHaveBeenCalled());
  await new Promise((r) => setTimeout(r, 0));
  return el;
}

describe("BlockedCards", () => {
  it("lists the blocked cards by project with the reason, the agent, and how long", async () => {
    active.mockResolvedValue([
      card({ id: "AB-2", project: "abc", title: "Spike", blocked_since: "2026-11-06T11:30:00Z" }),
      card({
        assignee: "A1",
        assignee_name: "terrarium/dash-ci",
        assignee_status: "blocked",
        blocked_since: "2026-11-06T09:55:00Z",
        blocked_reason: "need a target\nthe second line",
      }),
    ]);
    const el = await mount(Date.parse("2026-11-06T12:00:00Z"));
    expect(active).toHaveBeenCalledWith({ columns: ["blocked"] });
    await vi.waitFor(() => expect(el.querySelectorAll("tr[data-card]").length).toBe(2));

    expect(el.querySelectorAll("table").length).toBe(1);
    const cols = [...el.querySelectorAll("table > colgroup > col")].map((c) => c.className);
    expect(cols).toEqual(["col-id", "col-title", "col-reason", "col-agent", "col-since"]);
    const heads = [...el.querySelectorAll("tbody.blocked-project > tr.blocked-group > th")];
    expect(heads.map((h) => h.textContent)).toEqual(["abc", "terrarium"]);
    for (const h of heads) expect(h.getAttribute("colspan")).toBe("5");

    const row = el.querySelector('tr[data-card="TER-1"]')!;
    const cells = [...row.children].map((td) => td.textContent?.trim());
    expect(cells).toEqual(["TER-1", "Parser", "need a target", "terrarium/dash-ci blocked", "2h 05m"]);
    expect(row.querySelector(".blocked-reason")!.getAttribute("title")).toBe("need a target\nthe second line");
    const links = [...row.querySelectorAll("a")].map((a) => a.getAttribute("href"));
    expect(links).toEqual(["/p/terrarium/board?card=TER-1", "/agents/A1"]);

    const spike = [...el.querySelector('tr[data-card="AB-2"]')!.children].map((td) => td.textContent?.trim());
    expect(spike).toEqual(["AB-2", "Spike", "no comment", "unassigned", "30m"]);
    for (const tr of el.querySelectorAll("tr[data-card]")) expect(tr.children.length).toBe(5);
    expect(el.textContent).toContain("2 cards");
  });

  it("shows the empty state", async () => {
    active.mockResolvedValue([]);
    const el = await mount(Date.now());
    await vi.waitFor(() => expect(el.textContent).toContain("Nothing is blocked."));
  });
});
