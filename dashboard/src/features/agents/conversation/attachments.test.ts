import { describe, expect, it, vi } from "vitest";
import { canSend, imageLine, imageProblem, imagesFromFiles, imagesFromItems, submitTurn, turnText, type DraftImage, type ItemLike } from "./attachments";

const png = (name = "a.png", size = 4) => new File([new Uint8Array(size)], name, { type: "image/png" });
const item = (kind: string, type: string, file: File | null = null): ItemLike => ({ kind, type, getAsFile: () => file });
const draft = (file: File, id = file.name, path?: string): DraftImage => ({ id, file, state: path ? "uploaded" : "ready", path });

describe("composer images", () => {
  it("takes the image files of a paste and leaves the text and other files", () => {
    const a = png("a.png");
    const b = new File(["x"], "b.jpg", { type: "image/jpeg" });
    const items = [item("string", "text/plain"), item("file", "image/png", a), item("file", "application/pdf", new File(["%PDF"], "x.pdf")), item("file", "image/jpeg", b), item("file", "image/png", null)];
    expect(imagesFromItems(items)).toEqual([a, b]);
    expect(imagesFromItems(null)).toEqual([]);
    expect(imagesFromFiles([a, new File(["t"], "t.txt", { type: "text/plain" })])).toEqual([a]);
  });

  it("names each path on a line of its own, after the text", () => {
    const p1 = "/work/terra/w1/.terrarium/attachments/20261028-120000-abcdefgh.png";
    const p2 = "/work/terra/w1/.terrarium/attachments/20261028-120001-ijklmnop.jpg";
    expect(turnText("  look at this  ", [p1, p2])).toBe(`look at this\n\n${imageLine(p1)}\n${imageLine(p2)}`);
    expect(turnText("", [p1])).toBe(imageLine(p1));
    expect(turnText("hi", [])).toBe("hi");
    expect(imageLine(p1)).toContain(p1);
    expect(imageLine(p1).startsWith("[")).toBe(false);
    expect(canSend("", [draft(png())])).toBe(true);
    expect(canSend("  ", [])).toBe(false);
  });

  it("refuses types and sizes that the daemon refuses", () => {
    expect(imageProblem({ type: "image/svg+xml", size: 10 })).toMatch(/not a PNG/);
    expect(imageProblem({ type: "image/png", size: 21 * 1024 * 1024 })).toMatch(/20 MiB/);
    expect(imageProblem({ type: "image/png", size: 10 })).toBeNull();
  });

  it("uploads, then sends the text with the paths", async () => {
    const images = [draft(png("a.png")), draft(png("b.png"))];
    const attach = vi.fn(async (f: File) => ({ path: `/work/p/a/.terrarium/attachments/${f.name}` }));
    const send = vi.fn(async () => ({ accepted: true }));
    const onImage = vi.fn();
    const r = await submitTurn("see", images, { attach, send, onImage });
    expect(r.ok).toBe(true);
    expect(attach).toHaveBeenCalledTimes(2);
    expect(send).toHaveBeenCalledWith(`see\n\n${imageLine("/work/p/a/.terrarium/attachments/a.png")}\n${imageLine("/work/p/a/.terrarium/attachments/b.png")}`);
    expect(onImage).toHaveBeenCalledWith("a.png", { state: "uploaded", path: "/work/p/a/.terrarium/attachments/a.png" });
  });

  it("keeps the draft when an upload fails, and does not send", async () => {
    const images = [draft(png("a.png")), draft(png("b.png"))];
    const before = structuredClone(images.map(({ file: _f, ...rest }) => rest));
    const attach = vi.fn(async (f: File) => {
      if (f.name === "b.png") throw new Error("413 too_large");
      return { path: `/p/${f.name}` };
    });
    const send = vi.fn(async () => ({}));
    const patches: [string, Partial<DraftImage>][] = [];
    const r = await submitTurn("text", images, { attach, send, onImage: (id, p) => patches.push([id, p]) });
    expect(r.ok).toBe(false);
    expect(send).not.toHaveBeenCalled();
    // The draft objects stay as they were: the caller keeps them.
    expect(images.map(({ file: _f, ...rest }) => rest)).toEqual(before);
    expect(patches.at(-1)).toEqual(["b.png", { state: "failed", error: "413 too_large" }]);
  });

  it("does not upload an image again when the send failed after the uploads", async () => {
    const images = [draft(png("a.png"), "a", "/p/a.png"), draft(png("b.png"))];
    const attach = vi.fn(async () => ({ path: "/p/b.png" }));
    const send = vi.fn(async () => {
      throw new Error("not_running");
    });
    const r = await submitTurn("", images, { attach, send });
    expect(r).toMatchObject({ ok: false });
    expect(attach).toHaveBeenCalledTimes(1);
    expect(send).toHaveBeenCalledWith(`${imageLine("/p/a.png")}\n${imageLine("/p/b.png")}`);
  });

  it("refuses a bad file before the upload", async () => {
    const svg = new File(["<svg/>"], "x.svg", { type: "image/svg+xml" });
    const attach = vi.fn();
    const r = await submitTurn("", [draft(svg)], { attach, send: vi.fn() });
    expect(r.ok).toBe(false);
    expect(attach).not.toHaveBeenCalled();
  });
});
