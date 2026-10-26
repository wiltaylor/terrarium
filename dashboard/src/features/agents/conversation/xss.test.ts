// @vitest-environment jsdom
// Assistant text is agent-written markdown. The conversation view renders it
// with `assistantHtml` into `setSafeHtml` (the `Markdown` component). Every
// hostile payload must come out inert and must not run when mounted.
import { describe, expect, it } from "vitest";
import { ALLOWED_ATTR, ALLOWED_TAGS, isSafeHref, isSafeImageSrc, setSafeHtml } from "../../../lib/sanitize";
import { XSS_DOCUMENT, XSS_PAYLOADS } from "../../../lib/xss-payloads";
import { createMockApi } from "../../../api/mock";
import { assistantHtml, groupBlocks } from "./model";

const tags = new Set<string>(ALLOWED_TAGS);
const attrs = new Set<string>(ALLOWED_ATTR);

function problems(root: ParentNode): string[] {
  const out: string[] = [];
  for (const el of Array.from(root.querySelectorAll("*"))) {
    const tag = el.nodeName.toLowerCase();
    if (!tags.has(tag)) out.push(`tag <${tag}>`);
    for (const a of Array.from(el.attributes)) {
      const n = a.name.toLowerCase();
      if (!attrs.has(n) || n.startsWith("on")) out.push(`attr ${n} on <${tag}>`);
      if (n === "href" && !isSafeHref(a.value)) out.push(`href ${a.value}`);
      if (n === "src" && !isSafeImageSrc(a.value)) out.push(`src ${a.value}`);
    }
  }
  return out;
}

const mount = (text: string) => {
  const el = document.createElement("div");
  document.body.append(el);
  setSafeHtml(el, assistantHtml(text, "terrarium"));
  return el;
};

describe("assistant text renders inert", () => {
  for (const p of XSS_PAYLOADS) {
    it(p.name, () => {
      const el = mount(`Here is the output:\n\n${p.src}\n\nDone.`);
      expect(problems(el)).toEqual([]);
      el.remove();
    });
  }

  it("runs nothing when the whole hostile document mounts", async () => {
    const w = window as unknown as { __xss?: number };
    delete w.__xss;
    const el = mount(XSS_DOCUMENT);
    el.querySelectorAll("img").forEach((img) => img.dispatchEvent(new Event("error")));
    await new Promise((r) => setTimeout(r, 20));
    expect(w.__xss).toBeUndefined();
    expect(problems(el)).toEqual([]);
    el.remove();
  });

  it("the mock's hostile session renders inert through the same path", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const lead = await api.agents.get("terrarium/lead");
    const old = (await api.agents.sessions(lead.id)).items.find((s) => !s.current)!;
    const page = await api.agents.transcript(lead.id, old.id, { after: 0, limit: 500 });
    const texts = groupBlocks(page.blocks).filter((i) => i.type === "text");
    expect(texts.length).toBeGreaterThan(0);
    for (const t of texts) {
      if (t.type !== "text") continue;
      expect(t.block.text).toContain("<script>");
      const el = mount(t.block.text ?? "");
      expect(el.querySelector("script, iframe, svg, img[onerror]")).toBeNull();
      expect(problems(el)).toEqual([]);
      el.remove();
    }
  });
});
