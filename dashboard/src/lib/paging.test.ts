import { describe, expect, it, vi } from "vitest";
import { createHttpTransport } from "../api/http";
import { applyPage, collectAll, emptyPaged, mergePage, paginate } from "./paging";

const items = Array.from({ length: 7 }, (_, i) => ({ id: `I${i}` }));
const key = (x: { id: string }) => x.id;

describe("paginate (the daemon's cursor rule)", () => {
  it("cuts pages and gives the key of the last item as the cursor", () => {
    const p1 = paginate(items, { limit: 3 }, key);
    expect(p1.items.map(key)).toEqual(["I0", "I1", "I2"]);
    expect(p1.next).toBe("I2");
    const p2 = paginate(items, { limit: 3, cursor: p1.next! }, key);
    expect(p2.items.map(key)).toEqual(["I3", "I4", "I5"]);
    const p3 = paginate(items, { limit: 3, cursor: p2.next! }, key);
    expect(p3.items.map(key)).toEqual(["I6"]);
    expect(p3.next).toBeNull();
  });

  it("has no cursor when the page is exactly the rest", () => {
    expect(paginate(items, { limit: 7 }, key).next).toBeNull();
  });

  it("clamps the limit to 1..1000 and defaults to 100", () => {
    expect(paginate(items, { limit: 0 }, key).items).toHaveLength(1);
    const many = Array.from({ length: 1500 }, (_, i) => ({ id: String(i) }));
    expect(paginate(many, {}, key).items).toHaveLength(100);
    expect(paginate(many, { limit: 5000 }, key).items).toHaveLength(1000);
  });

  it("answers an empty page for an unknown cursor", () => {
    expect(paginate(items, { cursor: "nope" }, key)).toEqual({ items: [], next: null });
  });
});

describe("collectAll", () => {
  it("follows next until the last page", async () => {
    const fetchPage = vi.fn(async (cursor?: string) => paginate(items, { limit: 2, cursor }, key));
    expect((await collectAll(fetchPage)).map(key)).toEqual(items.map(key));
    expect(fetchPage).toHaveBeenCalledTimes(4);
    expect(fetchPage.mock.calls.map((c) => c[0])).toEqual([undefined, "I1", "I3", "I5"]);
  });

  it("stops at maxPages and on a cursor that does not move", async () => {
    const stuck = vi.fn(async () => ({ items: [{ id: "x" }], next: "same" }));
    expect(await collectAll(stuck, 10)).toHaveLength(2);
    const endless = vi.fn(async (c?: string) => ({ items: [{ id: c ?? "0" }], next: `${Number(c ?? 0) + 1}` }));
    expect(await collectAll(endless, 3)).toHaveLength(3);
  });
});

describe("load-more state", () => {
  it("appends pages without duplicates and resets on demand", () => {
    let s = applyPage(emptyPaged<{ id: string }>(), { items: items.slice(0, 3), next: "I2" }, key);
    expect(s.initial).toBe(false);
    s = applyPage(s, { items: items.slice(2, 5), next: null }, key);
    expect(s.items.map(key)).toEqual(["I0", "I1", "I2", "I3", "I4"]);
    expect(s.next).toBeNull();
    s = applyPage(s, { items: [items[6]!], next: null }, key, true);
    expect(s.items.map(key)).toEqual(["I6"]);
    expect(mergePage([{ id: "a" }], [{ id: "a" }, { id: "b" }], key).map(key)).toEqual(["a", "b"]);
  });
});

describe("transport.page", () => {
  it("returns the array and the X-Next-Cursor header", async () => {
    const f = vi.fn(async (_u: string | URL | Request, _i?: RequestInit) => new Response(JSON.stringify([{ id: 1 }]), { status: 200, headers: { "x-next-cursor": "01M5" } }));
    const t = createHttpTransport({ fetch: f, getToken: () => "t" });
    expect(await t.page("/api/agents", { limit: 1 })).toEqual({ items: [{ id: 1 }], next: "01M5" });
    expect(f.mock.calls[0]![0]).toBe("/api/agents?limit=1");
    const last = createHttpTransport({ fetch: vi.fn(async () => new Response("[]", { status: 200 })), getToken: () => null });
    expect(await last.page("/api/agents")).toEqual({ items: [], next: null });
  });

  it("refuses a body that is not an array", async () => {
    const t = createHttpTransport({ fetch: vi.fn(async () => new Response('{"items":[]}', { status: 200 })), getToken: () => null });
    await expect(t.page("/api/agents")).rejects.toThrow(/expected a JSON array/);
  });
});
