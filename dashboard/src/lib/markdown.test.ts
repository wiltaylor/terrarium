import { describe, expect, it } from "vitest";
import { renderWiki, resolveWikilink, safeUrl, splitFrontMatter, targetHref } from "./markdown";

const ctx = { project: "terrarium", pages: new Set(["index", "designs/runner-v2"]), versions: new Set(["demo-2026-10-31"]), cardPrefixes: new Set(["TER", "WEB"]), prefixProject: { TER: "terrarium", WEB: "webshop" } };

describe("resolveWikilink", () => {
  it("resolves every §15.1 form", () => {
    expect(resolveWikilink("designs/runner-v2", ctx)).toEqual({ kind: "page", project: "terrarium", path: "designs/runner-v2" });
    expect(resolveWikilink("TER-42", ctx)).toEqual({ kind: "card", project: "terrarium", id: "TER-42" });
    expect(resolveWikilink("WEB-4", ctx)).toEqual({ kind: "card", project: "webshop", id: "WEB-4" });
    expect(resolveWikilink("MEM-17", ctx)).toEqual({ kind: "memory", project: "terrarium", id: "MEM-17" });
    expect(resolveWikilink("v0.3.0", ctx)).toEqual({ kind: "version", project: "terrarium", name: "v0.3.0" });
    expect(resolveWikilink("demo-2026-10-31", ctx)).toEqual({ kind: "version", project: "terrarium", name: "demo-2026-10-31" });
    expect(resolveWikilink("project:other/reports/x", ctx)).toEqual({ kind: "page", project: "other", path: "reports/x" });
    expect(resolveWikilink("project:other", ctx)).toEqual({ kind: "page", project: "other", path: "index" });
  });

  it("treats an unknown card prefix as a page", () => {
    expect(resolveWikilink("ABC-1", ctx).kind).toBe("page");
  });

  it("builds dashboard hrefs", () => {
    expect(targetHref({ kind: "card", project: "terrarium", id: "TER-1" })).toBe("/p/terrarium/board?card=TER-1");
    expect(targetHref({ kind: "page", project: "a b", path: "x/y z" })).toBe("/p/a%20b/knowledge/wiki/x/y%20z");
    expect(targetHref({ kind: "memory", project: "terrarium", id: "MEM-17" })).toBe("/p/terrarium/knowledge/memory?id=MEM-17");
    expect(targetHref({ kind: "version", project: "terrarium", name: "v0.1.0" })).toBe("/p/terrarium/board/versions/v0.1.0");
  });
});

describe("renderWiki", () => {
  it("renders wikilinks and marks missing pages", () => {
    const html = renderWiki("See [[designs/runner-v2]] and [[nope]] and [[TER-12]].", ctx);
    expect(html).toContain('href="/p/terrarium/knowledge/wiki/designs/runner-v2"');
    expect(html).toMatch(/class="wikilink wikilink-page missing" href="\/p\/terrarium\/knowledge\/wiki\/nope"/);
    expect(html).toContain('href="/p/terrarium/board?card=TER-12"');
  });

  it("escapes raw HTML and drops unsafe URLs", () => {
    const html = renderWiki('<script>alert(1)</script>\n\n<img src=x onerror=alert(1)>\n\n[x](javascript:alert(1)) ![y](vbscript:z)', ctx);
    expect(html).not.toContain("<script");
    expect(html).not.toContain("<img src=x");
    expect(html).toContain("&lt;script&gt;");
    expect(html).not.toContain("javascript:");
    expect(html).not.toContain("vbscript:");
  });

  it("turns a db fence into a placeholder with the query as an escaped attribute", () => {
    const html = renderWiki('```db\ntype:bug status=open\n  sort:-severity  \n```\n', { project: "p" });
    expect(html).toContain('<div class="db-embed" data-db-query="type:bug status=open sort:-severity"></div>');
    const hostile = renderWiki('```db\ntype:bug title=""><img src=x onerror=alert(1)></div><script>x</script>"\n```\n', { project: "p" });
    expect(hostile).not.toContain("<img");
    expect(hostile).not.toContain("<script");
    expect(hostile).toContain("&quot;&gt;&lt;img src=x onerror=alert(1)&gt;&lt;/div&gt;&lt;script&gt;");
    // Another language stays a code block.
    expect(renderWiki("```dbx\nx\n```\n", { project: "p" })).toContain('<pre><code class="language-dbx">');
  });

  it("resolves artifact images, links, and fenced embeds", () => {
    const html = renderWiki("![d](artifact:ART-9)\n\n[r](artifact:ART-10)\n\n```artifact ART-11\n```\n", {
      ...ctx,
      artifactUrl: (id) => (id === "ART-9" ? "https://a.example/ART-9/d.png?sig=1" : id === "ART-10" ? "https://a.example/ART-10/r.txt" : undefined),
    });
    expect(html).toContain('<img src="https://a.example/ART-9/d.png?sig=1"');
    expect(html).toContain('href="https://a.example/ART-10/r.txt"');
    expect(html).toContain('data-artifact="ART-11"');
  });

  it("strips front matter from the output", () => {
    const html = renderWiki("---\nstatus: ready_for_review\n---\n# Title\n", ctx);
    expect(html).not.toContain("status:");
    expect(html).toContain("<h1");
  });
});

describe("helpers", () => {
  it("parses front matter", () => {
    expect(splitFrontMatter("---\nstatus: draft\nreviewer: wil\n---\nbody").meta).toEqual({ status: "draft", reviewer: "wil" });
    expect(splitFrontMatter("no meta").body).toBe("no meta");
  });
  it("allows only safe URL schemes", () => {
    expect(safeUrl("https://x")).toBe("https://x");
    expect(safeUrl("/p/x")).toBe("/p/x");
    expect(safeUrl("relative/page")).toBe("relative/page");
    expect(safeUrl("JavaScript:alert(1)")).toBeNull();
    expect(safeUrl("data:text/html,<b>")).toBeNull();
  });
});
