// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { renderWiki } from "./markdown";
import { ALLOWED_ATTR, ALLOWED_TAGS, isSafeHref, isSafeImageSrc, sanitizeHtml, sanitizeToFragment, setSafeHtml } from "./sanitize";
import { XSS_PAYLOADS } from "./xss-payloads";

const ctx = { project: "terrarium", pages: new Set(["index"]) };
const tags = new Set<string>(ALLOWED_TAGS);
const attrs = new Set<string>(ALLOWED_ATTR);

describe("communication blocks", () => {
  const render = (body: string) => {
    const root = document.createElement("div");
    setSafeHtml(root, renderWiki(body, ctx));
    return root;
  };
  it("renders the operator's SMS example", () => {
    const root = render("> [!CHAT] SMS with Sam Lee\n> **Sam Lee** · 2026-11-10 14:02\n> Are you free at 3?\n>\n> **Wil** · 14:05 · sent\n> Yes.\n>\n> **Wil** · draft\n> I will call you then. Is the **office number** right?");
    expect(root.querySelector(".comm-title")?.textContent).toBe("SMS with Sam Lee");
    expect(root.querySelectorAll(".comm-item")).toHaveLength(3);
    expect(root.querySelector(".comm-received")?.textContent).toContain("2026-11-10 14:02");
    expect(root.querySelector(".comm-sent")?.textContent).toContain("Yes.");
    expect(root.querySelector(".comm-draft p strong")?.textContent).toBe("office number");
  });
  it("renders the operator's invoice example", () => {
    const root = render("> [!EMAIL] Invoice 1042\n> From: Sam Lee <sam@example.com>\n> To: wil@example.com\n> Subject: Invoice 1042\n> Date: 2026-11-10 14:02\n> Status: received\n>\n> Hi Wil,\n> the invoice is attached.\n>\n> ---\n> From: wil@example.com\n> To: Sam Lee <sam@example.com>\n> Subject: Re: Invoice 1042\n> Status: draft\n>\n> Hi Sam,\n> I paid it today.");
    expect(root.querySelectorAll(".comm-item")).toHaveLength(2);
    expect(root.querySelector(".comm-received")?.textContent).toContain("Sam Lee <sam@example.com>");
    expect(root.querySelector(".comm-received p br")).not.toBeNull();
    expect(root.querySelector(".comm-draft")?.textContent).toContain("Draft, not sent");
    expect(root.querySelector(".comm-draft p")?.textContent).toContain("I paid it today.");
  });
  it("renders a chat with received, sent, draft and implicit received messages", () => {
    const root = render("> [!CHAT] Release discussion\n> **Sam** · 10:00 · received\n> Hello **team**\n>\n> **Lee** · 10:01 · sent\n> [Plan](https://example.com)\n> **Lee** · draft\n> Not ready\n> **Sam**\n> Thanks");
    expect(root.querySelector(".comm-title")?.textContent).toBe("Release discussion");
    expect(root.querySelectorAll(".comm-item")).toHaveLength(4);
    expect(root.querySelectorAll(".comm-received")).toHaveLength(2);
    expect(root.querySelector(".comm-sent a")?.getAttribute("href")).toBe("https://example.com");
    expect(root.querySelector(".comm-draft")?.textContent).toContain("Draft, not sent");
    expect(root.querySelector(".comm-received strong")?.textContent).toBe("Sam");
  });
  it("renders multiple mails, literal addresses and Markdown bodies", () => {
    const root = render("> [!EMAIL] Release mail\n> From: Sam Lee <sam@example.com>\n> To: Team\n> Cc: Jo\n> Subject: Release\n> Date: Today\n> Status: sent\n>\n> **Ready**\n>\n> ---\n> From: Jo\n> Status: draft\n>\n> Still `checking`\n>\n> ***\n>\n> More\n> ---\n> From: Team\n> Status: received\n>\n> Received\n> ---\n> From: Pat\n>\n> No status");
    expect(root.querySelectorAll(".comm-item")).toHaveLength(4);
    expect(root.querySelector(".comm-sent")?.textContent).toContain("Sam Lee <sam@example.com>");
    expect(root.querySelector(".comm-sent p strong")?.textContent).toBe("Ready");
    expect(root.querySelector(".comm-draft code")?.textContent).toBe("checking");
    expect(root.querySelector(".comm-draft hr")).not.toBeNull();
    expect(root.querySelectorAll(".comm-received")).toHaveLength(2);
  });
  it("preserves ordinary quotes and does not interpret fenced examples", () => {
    expect(render("> Ordinary quote").querySelector("blockquote")).not.toBeNull();
    expect(render("```\n> [!CHAT] Example\n> **Sam**\n```").querySelector(".comm-block")).toBeNull();
  });
  it("allows only the communication class names", () => {
    const html = sanitizeHtml('<div class="comm-block comm-chat comm-email comm-title comm-item comm-meta comm-draft comm-sent comm-received comm-evil"></div>');
    expect(html).toContain("comm-received");
    expect(html).not.toContain("comm-evil");
  });
  for (const payload of XSS_PAYLOADS) {
    it(`escapes communication fields and bodies: ${payload.name}`, () => {
      const value = payload.src.replace(/\n/g, " ");
      const root = render(`> [!EMAIL] ${value}\n> From: ${value}\n> Subject: ${value}\n>\n> ${value}\n\n> [!CHAT] ${value}\n> **${value}** · ${value} · draft\n> ${value}`);
      expect(root.querySelector("script, iframe, svg, math, object")).toBeNull();
      for (const el of root.querySelectorAll("*")) {
        for (const attr of el.attributes) expect(attr.name).not.toMatch(/^on/i);
      }
    });
  }
});

it("keeps syntax colors through the sanitizer and leaves unknown languages as text", () => {
  const root = document.createElement("div");
  setSafeHtml(root, renderWiki('```rust\nfn main() { println!("<script>"); }\n```', ctx));
  expect(root.querySelector(".hljs-keyword")?.textContent).toBe("fn");
  expect(root.querySelector("code")?.textContent).toBe('fn main() { println!("<script>"); }');
  setSafeHtml(root, renderWiki('```unknown\n<img src=x onerror=alert(1)>\n```', ctx));
  expect(root.querySelector("code")?.textContent).toBe("<img src=x onerror=alert(1)>");
  expect(root.querySelector("img")).toBeNull();
});

it("keeps hostile code inert when a registered grammar highlights it", () => {
  for (const payload of XSS_PAYLOADS) {
    expectInert(renderWiki(`\`\`\`html\n${payload.src}\n\`\`\``, ctx));
  }
});

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
    it(`card and memory Markdown + DOMPurify: ${p.name}`, () => expectInert(renderWiki(p.src, { ...ctx, textBody: true, cardKeys: { TER: "terrarium" } })));
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
    expect(out).toContain('<a class="wikilink wikilink-page" href="/p/terrarium/knowledge/wiki/index">');
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

  it("keeps the db placeholder; a hostile query stays text in the attribute", () => {
    const q = '"><img src=x onerror=alert(1)>';
    const html = renderWiki("```db\ntype:bug title=" + q + "\n```\n", ctx);
    expectInert(html);
    const box = document.createElement("div");
    setSafeHtml(box, html);
    const slot = box.querySelector<HTMLElement>("div.db-embed")!;
    expect(slot.dataset.dbQuery).toBe(`type:bug title=${q}`);
    expect(box.querySelector("img")).toBeNull();
    // A closing tag in the value: DOMPurify drops the attribute (its mXSS rule); nothing gets in.
    const closing = renderWiki("```db\ntype:bug title=</div><script>alert(2)</script>\n```\n", ctx);
    expectInert(closing);
    setSafeHtml(box, closing);
    expect(box.querySelector("script")).toBeNull();
    expect(box.querySelector<HTMLElement>("div.db-embed")!.dataset.dbQuery ?? "").not.toContain("<script");
    // Only on a div, and only up to 2000 characters.
    expect(sanitizeHtml('<span data-db-query="x">a</span>')).toBe("<span>a</span>");
    expect(sanitizeHtml(`<div class="db-embed" data-db-query="${"x".repeat(2001)}"></div>`)).toBe('<div class="db-embed"></div>');
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
