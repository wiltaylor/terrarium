import { describe, expect, it } from "vitest";
import type { AgentEvent, CardFile } from "../../api/types";
import { ACCEPT, existing, fileProblem, isCardFileEvent, MAX_FILE_BYTES, pastedFiles, pastedName, previewOf, rasterType, safeName } from "./cardFileRules";

const f = (name: string): CardFile => ({ card: "P-1", name, version: 1, size: 1, sha256: "x", media_type: "text/plain", created_by: "operator", created_by_name: "operator", created_at: "2026-11-04T00:00:00.000Z", versions: 1 });

describe("card files", () => {
  it("previews markdown, text, and raster images; never an SVG or a PDF", () => {
    expect(previewOf("spec.MD")).toBe("markdown");
    expect(previewOf("a.json")).toBe("text");
    expect(previewOf("a.yml")).toBe("text");
    expect(previewOf("shot.jpeg")).toBe("image");
    expect(previewOf("logo.svg")).toBe("none");
    expect(previewOf("doc.pdf")).toBe("none");
    expect(rasterType("a.webp")).toBe("image/webp");
    expect(rasterType("a.svg")).toBeNull();
  });

  it("makes safe names", () => {
    expect(safeName("My Notes (1).md")).toBe("My Notes -1-.md");
    expect(safeName("../../etc/passwd.txt")).toBe("passwd.txt");
    expect(safeName(".hidden.md")).toBe("hidden.md");
    expect(safeName("index.md")).toBe("index-1.md");
    expect(safeName("naïve.md")).toBe("na-ve.md");
    const long = safeName(`${"a".repeat(200)}.md`);
    expect(long.length).toBe(128);
    expect(long.endsWith(".md")).toBe(true);
  });

  it("names a pasted image by the time, in UTC", () => {
    expect(pastedName("image/png", new Date("2026-11-04T09:08:07Z"))).toBe("pasted-20261104-090807.png");
    expect(pastedName("image/jpeg", new Date("2026-11-04T09:08:07Z"))).toBe("pasted-20261104-090807.jpg");
    const png = new File(["x"], "image.png", { type: "image/png" });
    const doc = new File(["# x"], "Plan v2.md", { type: "text/markdown" });
    const items = [
      { kind: "string", type: "text/plain", getAsFile: () => null },
      { kind: "file", type: "image/png", getAsFile: () => png },
      { kind: "file", type: "text/markdown", getAsFile: () => doc },
    ];
    expect(pastedFiles(items, new Date("2026-11-04T09:08:07Z")).map((p) => p.name)).toEqual(["pasted-20261104-090807.png", "Plan v2.md"]);
  });

  it("checks the type and the size before an upload", () => {
    expect(fileProblem({ name: "a.md", size: 3 })).toBeNull();
    expect(fileProblem({ name: "run.sh", size: 3 })).toMatch(/takes only/);
    expect(fileProblem({ name: "a.md", size: 0 })).toMatch(/empty/);
    expect(fileProblem({ name: "a.png", size: MAX_FILE_BYTES + 1 })).toMatch(/10 MiB/);
    expect(ACCEPT).toContain(".pdf");
  });

  it("finds a file of the same name in any case, and the events of the card", () => {
    expect(existing([f("Spec.md")], "spec.MD")?.name).toBe("Spec.md");
    expect(existing([f("Spec.md")], "other.md")).toBeUndefined();
    const e = (kind: string, card?: string) => ({ kind, card }) as unknown as AgentEvent;
    expect(isCardFileEvent(e("card.file_added", "p-1"), "P-1")).toBe(true);
    expect(isCardFileEvent(e("card", "P-1"), "P-1")).toBe(false);
    expect(isCardFileEvent(e("card.file_removed", "P-2"), "P-1")).toBe(false);
  });
});
