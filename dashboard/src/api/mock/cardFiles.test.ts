// The card files of the mock answer as the daemon does (docs/api.md §8.4).
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import { ApiError } from "../http";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const blob = (s: string | Uint8Array<ArrayBuffer>, type = "") => new Blob([s], { type });
const code = async (p: Promise<unknown>) => {
  try {
    await p;
    return "ok";
  } catch (e) {
    return e instanceof ApiError ? `${e.status} ${e.code}` : String(e);
  }
};

async function card(api: ReturnType<typeof mock>) {
  return (await api.cards.create("terrarium", { title: "Files" })).id;
}

describe("mock card files", () => {
  it("seeds an assigned card with files and versions", async () => {
    const api = mock();
    const cards = (await api.cards.list("terrarium")).items.filter((c) => c.assignee);
    let found = false;
    for (const c of cards) {
      const files = await api.cardFiles.list(c.id);
      if (!files.length) continue;
      found = true;
      const brief = files.find((x) => x.name === "brief.md")!;
      expect(brief.version).toBe(2);
      expect(brief.versions).toBe(2);
      expect((await api.cardFiles.versions(c.id, "brief.md")).map((v) => v.version)).toEqual([2, 1]);
      const old = await api.cardFiles.get(c.id, "brief.md", 1);
      expect(await old.blob.text()).toContain("first draft");
      expect(files.map((x) => x.name)).toEqual(["brief.md", "grammar.json", "sketch.png"]);
    }
    expect(found).toBe(true);
  });

  it("adds, refuses a second name, replaces, keeps the same bytes, and removes", async () => {
    const api = mock();
    const id = await card(api);
    const a = await api.cardFiles.upload(id, blob("# A"), "spec.md");
    expect(a).toMatchObject({ name: "spec.md", version: 1, outcome: "added", media_type: "text/markdown; charset=utf-8", created_by: "operator" });
    expect(await code(api.cardFiles.upload(id, blob("x"), "SPEC.md"))).toBe("409 conflict");
    const b = await api.cardFiles.upload(id, blob("# B"), "spec.md", true);
    expect(b).toMatchObject({ version: 2, versions: 2, outcome: "replaced" });
    expect((await api.cardFiles.upload(id, blob("# B"), "spec.md", true)).outcome).toBe("unchanged");
    expect(await (await api.cardFiles.get(id, "spec.md")).blob.text()).toBe("# B");
    await api.cardFiles.remove(id, "spec.md");
    expect(await api.cardFiles.list(id)).toEqual([]);
    expect(await code(api.cardFiles.versions(id, "spec.md"))).toBe("404 not_found");
    expect((await api.cardFiles.upload(id, blob("# C"), "spec.md")).version).toBe(3);
    const ev = (await api.cards.events(id)).items.map((e) => e.kind).filter((k) => k.startsWith("card.file_"));
    expect(ev).toEqual(["card.file_added", "card.file_replaced", "card.file_removed", "card.file_added"]);
  });

  it("checks the names, the types, and the content", async () => {
    const api = mock();
    const id = await card(api);
    expect(await code(api.cardFiles.upload(id, blob("x"), ".hidden.md"))).toBe("422 invalid");
    expect(await code(api.cardFiles.upload(id, blob("x"), "index.md"))).toBe("422 invalid");
    expect(await code(api.cardFiles.upload(id, blob("x"), "a/b.md"))).toBe("422 invalid");
    expect(await code(api.cardFiles.upload(id, blob("x"), "run.sh"))).toBe("415 unsupported_media_type");
    expect(await code(api.cardFiles.upload(id, blob("GIF89a"), "fake.png"))).toBe("415 unsupported_media_type");
    expect(await code(api.cardFiles.upload(id, blob(new Uint8Array([0x61, 0, 0x62])), "nul.txt"))).toBe("415 unsupported_media_type");
    expect(await code(api.cardFiles.upload(id, blob("{nope"), "bad.json"))).toBe("422 invalid");
    expect(await code(api.cardFiles.upload(id, blob("<html></html>"), "not.svg"))).toBe("415 unsupported_media_type");
    expect(await code(api.cardFiles.upload(id, blob('<?xml version="1.0"?><svg xmlns="x"/>'), "ok.svg"))).toBe("ok");
    expect(await code(api.cardFiles.upload(id, blob("%PDF-1.7"), "ok.pdf"))).toBe("ok");
    expect(await code(api.cardFiles.upload(id, blob(new Uint8Array(10 * 1024 * 1024 + 1).fill(97)), "big.txt"))).toBe("413 too_large");
    await api.cards.archive(id);
    expect(await code(api.cardFiles.upload(id, blob("x"), "late.md"))).toBe("409 conflict");
  });
});
