import { describe, expect, it } from "vitest";
import { cardOfHref, renderWiki, resolveWikilink, safeUrl, splitFrontMatter, targetHref } from "./markdown";

const ctx = { project: "terrarium", pages: new Set(["index", "designs/runner-v2"]), cardPrefixes: new Set(["TER", "WEB"]), prefixProject: { TER: "terrarium", WEB: "webshop" } };

describe("GitHub emoji", () => {
  it("expands only known names in wiki and text bodies", () => {
    for (const textBody of [false, true]) {
      const html = renderWiki(":white_check_mark: :+1: :not_a_real_emoji: file.rs:143: :constructor:", { ...ctx, textBody });
      expect(html).toContain("✅ 👍 :not_a_real_emoji: file.rs:143: :constructor:");
    }
  });
  it("leaves code spans, fenced and indented code unchanged", () => {
    const html = renderWiki("`:smile:`\n\n```\n:smile:\n```\n\n    :smile:", ctx);
    expect(html).not.toContain("😄");
    expect(html.match(/:smile:/g)).toHaveLength(3);
  });
  it("does not expand URLs, but expands link labels", () => {
    const html = renderWiki("https://example.org/:smile: [:+1:](https://example.org/:smile:) <https://example.org/:smile:>", ctx);
    expect(html).not.toContain("😄");
    expect(html).toContain(">👍</a>");
    expect(html.match(/href="https:\/\/example.org\/:smile:"/g)).toHaveLength(3);
  });
});

describe("card and memory text", () => {
  const render = (src: string) => renderWiki(src, { ...ctx, textBody: true, cardKeys: ctx.prefixProject });
  it("keeps single newlines local to text bodies", () => {
    expect(render("first\nsecond")).toContain("first<br>second");
    expect(renderWiki("first\nsecond", ctx)).not.toContain("<br>");
  });
  it("preserves legacy placeholders, paths, globs and indented lines", () => {
    const html = render("Use <id> at src/app.rs:42\nRun crates/*/tests/*.rs\n\n    cargo test --workspace");
    expect(html).toContain("&lt;id&gt;");
    expect(html).toContain("src/app.rs:42");
    expect(html).toContain("crates/*/tests/*.rs");
    expect(html).toContain("<pre><code>cargo test --workspace</code></pre>");
    expect(render("---\nstatus: ready\n---\nbody")).toContain("status: ready");
  });
  it("uses GFM tables, fences and reader-aware links", () => {
    const html = render("| A | B |\n| - | - |\n| x | y |\n\n```sh\nls *.rs\n```\n\n[[index]] TER-12 [card](card:WEB-4) [site](https://example.com)");
    expect(html).toContain("<table>");
    expect(html).toContain('<code class="language-sh">');
    expect(html).toContain('/p/terrarium/knowledge/wiki/index');
    expect(html).toContain('/p/terrarium/board?card=TER-12');
    expect(html).toContain('/p/webshop/board?card=WEB-4');
    expect(html).toContain('href="https://example.com"');
  });
});

describe("resolveWikilink", () => {
  it("resolves every §15.1 form", () => {
    expect(resolveWikilink("designs/runner-v2", ctx)).toEqual({ kind: "page", project: "terrarium", path: "designs/runner-v2" });
    expect(resolveWikilink("TER-42", ctx)).toEqual({ kind: "card", project: "terrarium", id: "TER-42" });
    expect(resolveWikilink("WEB-4", ctx)).toEqual({ kind: "card", project: "webshop", id: "WEB-4" });
    expect(resolveWikilink("MEM-17", ctx)).toEqual({ kind: "memory", project: "terrarium", id: "MEM-17" });
    // A version is a card label since TER-58: `[[v0.3.0]]` is a page, as in the daemon.
    expect(resolveWikilink("v0.3.0", ctx)).toEqual({ kind: "page", project: "terrarium", path: "v0.3.0" });
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
  it("refuses a URL with a user name or password, as LinkText does (TER-598)", () => {
    for (const h of ["https://user:pw@example.com/", "http://user@example.com/", "HTTPS://a:b@example.com", "https:user:pw@example.com", "//user:pw@example.com/x", "/\\user:pw@example.com/x"]) {
      expect(safeUrl(h), h).toBeNull();
    }
    expect(safeUrl("https://example.com/a@b")).toBe("https://example.com/a@b");
    expect(safeUrl("mailto:a@b.c")).toBe("mailto:a@b.c");
    expect(safeUrl("/p/x@y")).toBe("/p/x@y");
    expect(renderWiki("[x](https://user:pw@example.com/) <https://u:p@example.com/>", { project: "p" })).not.toContain("<a");
  });
});

describe("renderWiki cardKeys (TER-571)", () => {
  const keys = { TER: "terrarium", WEB: "webshop" };
  const md = (src: string) => renderWiki(src, { project: "terrarium", cardKeys: keys });

  it("links a bare card key of the reader's projects", () => {
    const html = md("Blocked by TER-12, WEB-4 and ABC-1.");
    expect(html).toContain('<a class="wikilink wikilink-card" href="/p/terrarium/board?card=TER-12">TER-12</a>');
    expect(html).toContain('href="/p/webshop/board?card=WEB-4"');
    expect(html).not.toContain("ABC-1<");
    expect(html).toContain("ABC-1.");
  });

  it("does not link a bare key inside a word, a link, or code", () => {
    const html = md("XTER-1 a/TER-2 `TER-3` [TER-4 fix](https://example.com/TER-5)");
    expect(html).not.toContain("card=TER-1");
    expect(html).not.toContain("card=TER-2");
    expect(html).not.toContain("card=TER-3");
    expect(html).not.toContain("card=TER-4");
    expect(html).toContain('<a href="https://example.com/TER-5" target="_blank" rel="noopener noreferrer">TER-4 fix</a>');
  });

  it("renders card: and wiki: links, and refuses a bad one as its text", () => {
    expect(md("[the fix](card:TER-7)")).toContain('<a class="wikilink wikilink-card" href="/p/terrarium/board?card=TER-7">the fix</a>');
    expect(md("[the design](wiki:designs/x)")).toContain('<a class="wikilink wikilink-page" href="/p/terrarium/knowledge/wiki/designs/x">the design</a>');
    const bad = md("[a](card:ABC-1) [b](wiki:../x) [c](javascript:alert(1)) [d](wiki:project:../x) [e](wiki:project:./x) [f](wiki:project:%2e%2e/x)");
    expect(bad).not.toContain("<a");
  });

  it("opens bare URLs in a new tab", () => {
    expect(md("See https://example.com/x.")).toContain('<a href="https://example.com/x" target="_blank" rel="noopener noreferrer">');
  });

  it("changes nothing without cardKeys", () => {
    expect(renderWiki("TER-1 [x](card:TER-1)", { project: "terrarium" })).toBe("<p>TER-1 x</p>\n");
  });
});

describe("cardOfHref", () => {
  it("reads the card of a card link, and nothing else", () => {
    expect(cardOfHref("/p/a%20b/board?card=TER-1")).toEqual({ project: "a b", id: "TER-1" });
    expect(cardOfHref("/p/x/knowledge/wiki/TER-1")).toBeNull();
    expect(cardOfHref("https://evil.example/p/x/board?card=TER-1")).toBeNull();
  });
});
