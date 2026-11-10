// Links in plain text (GitHub #31, TER-571): each syntax, the refused schemes,
// and card keys only for the reader's projects.

import { describe, expect, it } from "vitest";
import { externalHref, type LinkSegment, linkSegments } from "./linkText";

const ctx = { project: "terrarium", prefixProject: { TER: "terrarium", WEB: "webshop" } };
const seg = (text: string) => linkSegments(text, ctx);
const links = (text: string) => seg(text).filter((s): s is Exclude<LinkSegment, { kind: "text" }> => s.kind !== "text");
const plain = (text: string) => expect(seg(text)).toEqual([{ kind: "text", text }]);

describe("linkSegments", () => {
  it("keeps text without links as one text segment", () => {
    plain("No links here, only TER and 42.");
    expect(seg("")).toEqual([]);
  });

  it("links a Markdown external link", () => {
    expect(seg("See [the issue](https://github.com/a/b/issues/31) now.")).toEqual([
      { kind: "text", text: "See " },
      { kind: "external", text: "the issue", href: "https://github.com/a/b/issues/31" },
      { kind: "text", text: " now." },
    ]);
  });

  it("links a bare http(s) URL and leaves the end of the sentence out", () => {
    expect(seg("Read https://example.com/a?b=1.")).toEqual([
      { kind: "text", text: "Read " },
      { kind: "external", text: "https://example.com/a?b=1", href: "https://example.com/a?b=1" },
      { kind: "text", text: "." },
    ]);
    expect(links("(see http://example.com/x)")).toEqual([{ kind: "external", text: "http://example.com/x", href: "http://example.com/x" }]);
    expect(links("https://en.wikipedia.org/wiki/Foo_(bar), ok")[0]!.text).toBe("https://en.wikipedia.org/wiki/Foo_(bar)");
  });

  it("refuses every scheme but http and https", () => {
    for (const href of ["javascript:alert(1)", "JavaScript:alert(1)", "data:text/html,<b>x</b>", "vbscript:x", "file:///etc/passwd", "mailto:a@b.c", "//evil.example/x", "/relative"]) {
      plain(`[click](${href})`);
    }
    plain("javascript:alert(1) data:text/html,x");
    expect(externalHref("javascript://example.com/%0aalert(1)")).toBeNull();
    expect(externalHref("https://user:pw@example.com/")).toBeNull();
    expect(externalHref("https://")).toBeNull();
  });

  it("links wiki pages: [text](wiki:page) and [[page]]", () => {
    expect(links("[the design](wiki:designs/runner-v2)")).toEqual([{ kind: "wiki", text: "the design", href: "/p/terrarium/knowledge/wiki/designs/runner-v2" }]);
    expect(links("See [[designs/runner v2]].")).toEqual([{ kind: "wiki", text: "designs/runner v2", href: "/p/terrarium/knowledge/wiki/designs/runner%20v2" }]);
    expect(links("[[project:webshop/notes]]")).toEqual([{ kind: "wiki", text: "project:webshop/notes", href: "/p/webshop/knowledge/wiki/notes" }]);
    expect(links("[x](wiki:a%20b)")[0]!.href).toBe("/p/terrarium/knowledge/wiki/a%20b");
  });

  it("refuses a wiki path with an empty, `.`, or `..` segment", () => {
    plain("[x](wiki:../../settings)");
    plain("[[a/../b]]");
    plain("[[a//b]]");
    plain("[x](wiki:%E0%A4%A)");
  });

  it("refuses an empty, `.`, or `..` project part (TER-598)", () => {
    plain("[[project:../x]]");
    plain("[[project:./x]]");
    plain("[[project:..]]");
    plain("[[project:/x]]");
    plain("[x](wiki:project:../x)");
    plain("[x](wiki:project:%2E%2E/x)");
    expect(links("[[project:webshop]]")).toEqual([{ kind: "wiki", text: "project:webshop", href: "/p/webshop/knowledge/wiki/index" }]);
  });

  it("links card keys of the reader's projects: bare, [text](card:KEY), and [[KEY]]", () => {
    expect(seg("Blocked by TER-12 and WEB-4.")).toEqual([
      { kind: "text", text: "Blocked by " },
      { kind: "card", text: "TER-12", href: "/p/terrarium/board?card=TER-12", project: "terrarium", id: "TER-12" },
      { kind: "text", text: " and " },
      { kind: "card", text: "WEB-4", href: "/p/webshop/board?card=WEB-4", project: "webshop", id: "WEB-4" },
      { kind: "text", text: "." },
    ]);
    expect(links("[the fix](card:TER-7)")).toEqual([{ kind: "card", text: "the fix", href: "/p/terrarium/board?card=TER-7", project: "terrarium", id: "TER-7" }]);
    expect(links("[[TER-9]]")).toEqual([{ kind: "card", text: "TER-9", href: "/p/terrarium/board?card=TER-9", project: "terrarium", id: "TER-9" }]);
  });

  it("leaves a card key of another project, or a key inside a word, as text", () => {
    plain("ABC-12 is not ours.");
    plain("[x](card:ABC-12)");
    plain("[x](card:TER-x)");
    plain("XTER-1 TER-1a a/TER-1 TER-1_b");
    plain("toString-1 constructor-1");
    expect(linkSegments("TER-1", { project: "terrarium", prefixProject: {} })).toEqual([{ kind: "text", text: "TER-1" }]);
  });

  it("does not link a card key inside a URL", () => {
    expect(links("https://github.com/o/r/pull/1#TER-5")).toEqual([{ kind: "external", text: "https://github.com/o/r/pull/1#TER-5", href: "https://github.com/o/r/pull/1#TER-5" }]);
  });

  it("keeps HTML in the text as text: the segments are text, never markup", () => {
    const text = '<img src=x onerror=alert(1)> [<b>x</b>](https://example.com/"><script>) TER-1';
    const out = seg(text);
    expect(out[0]).toEqual({ kind: "text", text: "<img src=x onerror=alert(1)> " });
    const ext = out.find((s) => s.kind === "external")!;
    expect(ext.text).toBe("<b>x</b>");
    expect(ext.href).toBe("https://example.com/%22%3E%3Cscript%3E");
  });

  it("keeps the text around links", () => {
    const text = "a https://x.example/ b [[p]] c TER-3 d";
    expect(seg(text).map((s) => s.text).join("")).toBe("a https://x.example/ b p c TER-3 d");
  });
});
