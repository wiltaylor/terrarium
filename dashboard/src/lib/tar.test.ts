import { describe, expect, it } from "vitest";
import { buildTar, bundlePaths, splitPath } from "./tar";

const dec = new TextDecoder();
const field = (b: Uint8Array, off: number, len: number) => dec.decode(b.subarray(off, off + len)).replace(/\0.*$/s, "");

describe("buildTar", () => {
  it("writes ustar headers, padded bodies, and two end blocks", () => {
    const tar = buildTar([
      { path: "index.html", data: new TextEncoder().encode("<h1>hi</h1>") },
      { path: "css/site.css", data: new Uint8Array(600) },
    ]);
    expect(tar.length).toBe(512 + 512 + 512 + 1024 + 1024);
    expect(field(tar, 0, 100)).toBe("index.html");
    expect(field(tar, 124, 12)).toBe((11).toString(8).padStart(11, "0"));
    expect(field(tar, 257, 6)).toBe("ustar");
    expect(dec.decode(tar.subarray(512, 523))).toBe("<h1>hi</h1>");
    expect(field(tar, 1024, 100)).toBe("css/site.css");
    // checksum: the sum of the header bytes with the field as spaces
    const h = tar.slice(0, 512);
    const stored = parseInt(field(h, 148, 8), 8);
    h.fill(0x20, 148, 156);
    expect(h.reduce((n, b) => n + b, 0)).toBe(stored);
    expect(tar.subarray(tar.length - 1024).every((b) => b === 0)).toBe(true);
  });

  it("splits long paths into prefix and name", () => {
    const long = `${"d".repeat(120)}/file.txt`;
    expect(splitPath(long)).toEqual({ prefix: "d".repeat(120), name: "file.txt" });
    expect(() => splitPath("x".repeat(300))).toThrow(/too long/);
  });

  it("drops the shared top folder of a folder pick", () => {
    expect(bundlePaths(["site/index.html", "site/a/b.js"])).toEqual(["index.html", "a/b.js"]);
    expect(bundlePaths(["a.txt", "b.txt"])).toEqual(["a.txt", "b.txt"]);
  });
});
