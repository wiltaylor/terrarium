// @vitest-environment jsdom
// The wiki `db` block: each view, and hostile values that stay text.
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { DbQueryResult } from "../api/types";

const HOSTILE = '<img src=x onerror="window.__xss=1"><script>window.__xss=2</script>';
let answer: DbQueryResult | Error;
const query = vi.fn(async () => {
  if (answer instanceof Error) throw answer;
  return answer;
});
vi.mock("../stores", async () => { const { instanceTestStores } = await import("../test/instance"); return instanceTestStores(({ api: { db: { query: (...a: unknown[]) => (query as (...x: unknown[]) => unknown)(...a) } }, events: { on: () => () => {} } })); });
const { DbEmbed } = await import("./DbEmbed");

const rec = (id: string, data: Record<string, unknown>) => ({ id, project: "p", type: "bug", data, revision: 1, created_by: "operator", created_by_name: "operator", updated_by: "operator", updated_by_name: "operator", created_at: "2026-10-01T00:00:00.000Z", updated_at: "2026-10-01T00:00:00.000Z" });
const base = (over: Partial<DbQueryResult>): DbQueryResult => ({
  type: "bug",
  query: "q",
  view: "table",
  fields: [
    { name: "title", kind: "string", required: true },
    { name: "service", kind: "ref", ref: "service", required: false },
  ],
  records: [rec("R1", { title: HOSTILE, service: "S1" })],
  count: 1,
  labels: { S1: HOSTILE },
  ...over,
});

let dispose: (() => void) | undefined;
afterEach(() => dispose?.());

async function mount(q: string) {
  const host = document.createElement("div");
  document.body.append(host);
  dispose = render(() => createComponent(DbEmbed, { project: "p", query: q }), host);
  await new Promise((r) => setTimeout(r, 10));
  return host;
}

describe("DbEmbed", () => {
  it("renders a table with text cells and a ref link; hostile values stay text", async () => {
    answer = base({});
    const host = await mount(HOSTILE);
    expect(query).toHaveBeenLastCalledWith("p", HOSTILE);
    expect(host.querySelector("img, script")).toBeNull();
    expect((window as unknown as { __xss?: number }).__xss).toBeUndefined();
    const cells = Array.from(host.querySelectorAll("td")).map((td) => td.textContent);
    expect(cells).toEqual([HOSTILE, HOSTILE]);
    const link = host.querySelector<HTMLAnchorElement>("td a")!;
    expect(link.getAttribute("href")).toBe("/p/p/knowledge/db/service?id=S1");
    expect(host.querySelector(".db-embed-query")!.textContent).toBe(HOSTILE);
    expect(Array.from(host.querySelectorAll("a")).find((a) => a.textContent === "Open in DB")!.getAttribute("href")).toBe(`/p/p/knowledge/db/bug?q=${encodeURIComponent(HOSTILE).replace(/%20/g, "+")}`);
  });

  it("renders the list, count, and group views", async () => {
    answer = base({ view: "list" });
    let host = await mount("type:bug view:list");
    expect(host.querySelector("ul.db-list li")!.textContent).toContain(HOSTILE);
    dispose!();
    answer = base({ view: "count", records: [], count: 7 });
    host = await mount("type:bug view:count");
    expect(host.querySelector(".db-count")!.textContent).toBe("7");
    dispose!();
    answer = base({ view: "group", records: [], group: "severity", groups: [{ value: "high", count: 3 }, { value: null, count: 1 }] });
    host = await mount("type:bug group:severity");
    expect(Array.from(host.querySelectorAll("tbody td")).map((t) => t.textContent)).toEqual(["high", "3", "(none)", "1"]);
  });

  it("shows an error as a muted line", async () => {
    answer = new Error("bad query: position 9: no field");
    const host = await mount("type:bug x=1");
    expect(host.querySelector(".db-embed-error")!.textContent).toBe("DB query failed: bad query: position 9: no field");
  });
});
