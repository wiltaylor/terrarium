// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { renderWiki } from "./markdown";
import { ALLOWED_ATTR, ALLOWED_TAGS, isSafeHref, isSafeImageSrc, sanitizeHtml, sanitizeToFragment, setSafeHtml } from "./sanitize";
import { XSS_PAYLOADS } from "./xss-payloads";

const ctx = { project: "terrarium", pages: new Set(["index"]) };
const tags = new Set<string>(ALLOWED_TAGS);
const attrs = new Set<string>(ALLOWED_ATTR);

/** Fail with a reason when anything in `root` could run script or leave the page. */
function inertProblems(root: ParentNode): string[] {
  const out: string[] = [];
  for (const el of Array.from(root.querySelectorAll("*"))) {
    const tag = el.nodeName.toLowerCase();
    if (!tags.has(tag)) out.push(`tag <${tag}>`);
    if (el.namespaceURI !== "http://www.w3.org/1999/xhtml") out.push(`namespace ${el.namespaceURI} on <${tag}>`);
    for (const a of Array.from(el.attributes)) {
      const n = a.name.toLowerCase();
      if (!attrs.has(n)) out.push(`attr ${n} on <${tag}>`);
      if (n.startsWith("on")) out.push(`handler ${n}`);
      if (n === "href" && !isSafeHref(a.value)) out.push(`href ${a.value}`);
      if (n === "src" && !isSafeImageSrc(a.value)) out.push(`src ${a.value}`);
      if (n === "target" && el.getAttribute("rel") !== "noopener noreferrer") out.push("target without rel");
    }
  }
  return out;
}

/** Check the fragment, then serialise and parse it again (the mXSS round trip). */
function expectInert(html: string) {
  const frag = sanitizeToFragment(html);
  expect(inertProblems(frag)).toEqual([]);
  const box = document.createElement("div");
  box.append(frag);
  const again = document.createElement("template");
  again.innerHTML = box.innerHTML;
  expect(inertProblems(again.content)).toEqual([]);
  expect(inertProblems(document.createRange().createContextualFragment(sanitizeHtml(html)))).toEqual([]);
}

describe("sanitizer: adversarial payloads", () => {
  for (const p of XSS_PAYLOADS) {
    it(`renderer + DOMPurify: ${p.name}`, () => expectInert(renderWiki(p.src, ctx)));
    it(`DOMPurify alone: ${p.name}`, () => expectInert(p.src));
  }

  it("has a checker that flags the raw payloads", () => {
    const raw = (s: string) => inertProblems(document.createRange().createContextualFragment(s));
    for (const s of ["<img src=x onerror=1>", "<svg></svg>", '<a href="java&#x09;script:1">x</a>', '<a href="data:text/html,x">x</a>', '<a href="https://x" target="_blank">x</a>', "<iframe></iframe>"]) {
      expect(raw(s)).not.toEqual([]);
    }
  });

  it("does not run any payload when mounted", async () => {
    const w = window as unknown as { __xss?: number };
    delete w.__xss;
    const host = document.createElement("div");
    document.body.append(host);
    for (const p of XSS_PAYLOADS) {
      setSafeHtml(host, p.src);
      setSafeHtml(host, renderWiki(p.src, ctx));
    }
    await new Promise((r) => setTimeout(r, 20));
    expect(w.__xss).toBeUndefined();
    host.remove();
  });
});

describe("sanitizer: what the renderer emits survives", () => {
  it("keeps wikilinks, headings, lists, tables, code, and task lists", () => {
    const md = "# T\n\n[[index]] [[nope]]\n\n- [x] done\n- [ ] todo\n\n| a | b |\n|:-|-:|\n| 1 | 2 |\n\n```rust\nfn main() {}\n```\n\n3. three\n4. four\n\n> quote ~~old~~ **bold** _em_ `code`\n\n---\n";
    const out = sanitizeHtml(renderWiki(md, ctx));
    expect(out).toContain('<a class="wikilink wikilink-page" href="/p/terrarium/wiki/index">');
    expect(out).toContain('class="wikilink wikilink-page missing"');
    expect(out).toMatch(/<input (checked="" disabled=""|disabled="" checked="") type="checkbox">/);
    expect(out).toContain('<th align="left">');
    expect(out).toContain('<code class="language-rust">');
    expect(out).toContain('<ol start="3">');
    expect(out).toContain("<del>old</del>");
    expect(out).toContain("<hr>");
  });

  it("keeps artifact placeholders and links, and sets rel on external links", () => {
    const out = sanitizeHtml(
      renderWiki("```artifact ART-11\n```\n\n[r](artifact:ART-10) [e](https://example.com/x)", {
        ...ctx,
        artifactUrl: () => "https://a.example/ART-10/r.txt?sig=1",
      }),
    );
    expect(out).toContain('<div class="artifact-embed" data-artifact="ART-11"></div>');
    expect(out).toContain('data-artifact-link="ART-10"');
    expect(out).toContain('href="https://example.com/x" target="_blank" rel="noopener noreferrer"');
  });

  it("allows base64 PNG, JPEG, and GIF images only", () => {
    expect(sanitizeHtml('<img src="data:image/png;base64,iVBORw0KGgo=">')).toContain("data:image/png");
    expect(sanitizeHtml('<img src="data:image/gif;base64,R0lGOD==">')).toContain("data:image/gif");
    expect(sanitizeHtml('<img src="data:image/svg+xml;base64,PHN2Zz4=">')).not.toContain("data:");
    expect(sanitizeHtml('<img src="data:image/png,raw">')).not.toContain("data:");
    expect(sanitizeHtml('<a href="data:image/png;base64,iVBO">x</a>')).not.toContain("data:");
  });

  it("drops class names the renderer does not make, and the target of internal links", () => {
    expect(sanitizeHtml('<p class="primary danger wikilink">x</p>')).toBe('<p class="wikilink">x</p>');
    expect(sanitizeHtml('<a href="/p/x" target="_top" rel="opener">x</a>')).toBe('<a href="/p/x">x</a>');
  });
});

describe("URL checks", () => {
  it("normalises whitespace and controls before it reads the scheme", () => {
    expect(isSafeHref("java\tscript:x")).toBe(false);
    expect(isSafeHref(" \u0001javascript:x")).toBe(false);
    expect(isSafeHref("//evil.example/x")).toBe(false);
    expect(isSafeHref("/\\evil.example")).toBe(false);
    expect(isSafeHref("https://x")).toBe(true);
    expect(isSafeHref("mailto:a@b")).toBe(true);
    expect(isSafeHref("#top")).toBe(true);
    expect(isSafeHref("page/sub")).toBe(true);
    expect(isSafeImageSrc("data:image/jpeg;base64,/9j/")).toBe(true);
    expect(isSafeImageSrc("data:text/html;base64,PHM+")).toBe(false);
    expect(isSafeImageSrc("mailto:a@b")).toBe(false);
  });
});
