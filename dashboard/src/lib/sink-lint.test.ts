// The ESLint sink rule (eslint.config.js, docs/security-review.md F3 and
// F14): every way to put a string into the DOM as HTML outside lib/sanitize
// must fail the lint.

import { describe, expect, it } from "vitest";
import { ESLint } from "eslint";

const eslint = new ESLint({ cwd: new URL("../..", import.meta.url).pathname });

/** Lint `code` as if it were a file of the app. The path must be in tsconfig for the typed parser. */
async function sinkErrors(code: string, filePath: string): Promise<string[]> {
  const [res] = await eslint.lintText(code, { filePath });
  return res!.messages.filter((m) => m.ruleId === "no-restricted-syntax").map((m) => m.message);
}

const TS = "src/lib/format.ts";
const TSX = "src/components/StatusBadge.tsx";

const flagged: { name: string; code: string; file: string }[] = [
  { name: "el.innerHTML =", code: `export const f = (el: HTMLElement, x: string) => { el.innerHTML = x; };`, file: TS },
  { name: "el.outerHTML =", code: `export const f = (el: HTMLElement, x: string) => { el.outerHTML = x; };`, file: TS },
  { name: 'el["innerHTML"] =', code: `export const f = (el: HTMLElement, x: string) => { el["innerHTML"] = x; };`, file: TS },
  { name: "el[`innerHTML`] =", code: "export const f = (el: HTMLElement, x: string) => { el[`innerHTML`] = x; };", file: TS },
  { name: 'frame["srcdoc"] =', code: `export const f = (el: HTMLIFrameElement, x: string) => { el["srcdoc"] = x; };`, file: TS },
  { name: "frame.srcdoc =", code: `export const f = (el: HTMLIFrameElement, x: string) => { el.srcdoc = x; };`, file: TS },
  { name: "Object.assign(el, { innerHTML })", code: `export const f = (el: HTMLElement, innerHTML: string) => Object.assign(el, { innerHTML });`, file: TS },
  { name: 'Object.assign(el, { "outerHTML": x })', code: `export const f = (el: HTMLElement, x: string) => Object.assign(el, { "outerHTML": x });`, file: TS },
  { name: 'Reflect.set(el, "innerHTML", x)', code: `export const f = (el: HTMLElement, x: string) => Reflect.set(el, "innerHTML", x);`, file: TS },
  { name: 'setAttribute("srcdoc", x)', code: `export const f = (el: HTMLElement, x: string) => el.setAttribute("srcdoc", x);`, file: TS },
  { name: "new DOMParser()", code: `export const p = new DOMParser();`, file: TS },
  { name: "parseFromString", code: `export const f = (p: DOMParser, x: string) => p.parseFromString(x, "text/html");`, file: TS },
  { name: "insertAdjacentHTML", code: `export const f = (el: HTMLElement, x: string) => el.insertAdjacentHTML("beforeend", x);`, file: TS },
  { name: "createContextualFragment", code: `export const f = (r: Range, x: string) => r.createContextualFragment(x);`, file: TS },
  { name: "setHTMLUnsafe", code: `export const f = (el: { setHTMLUnsafe(s: string): void }, x: string) => el.setHTMLUnsafe(x);`, file: TS },
  { name: "JSX innerHTML", code: `export const C = (p: { x: string }) => <div innerHTML={p.x} />;`, file: TSX },
  { name: "JSX prop:innerHTML", code: `export const C = (p: { x: string }) => <div prop:innerHTML={p.x} />;`, file: TSX },
  { name: "JSX attr:srcdoc", code: `export const C = (p: { x: string }) => <iframe attr:srcdoc={p.x} />;`, file: TSX },
  { name: "JSX srcdoc", code: `export const C = (p: { x: string }) => <iframe srcdoc={p.x} />;`, file: TSX },
  { name: "document.write", code: `export const f = (x: string) => document.write(x);`, file: TS },
];

describe("the sink lint rule", () => {
  it.each(flagged)("flags $name", async ({ code, file }) => {
    expect(await sinkErrors(code, file)).not.toEqual([]);
  }, 30_000);

  it("leaves safe code alone", async () => {
    const safe = [
      `export const f = (el: HTMLElement, x: string) => { el.textContent = x; el.setAttribute("title", x); };`,
      `export const g = (a: object) => Object.assign({}, a, { title: "x" });`,
    ].join("\n");
    expect(await sinkErrors(safe, TS)).toEqual([]);
    expect(await sinkErrors(`export const C = (p: { x: string }) => <iframe sandbox="allow-scripts" src={p.x} title="t" />;`, TSX)).toEqual([]);
  }, 30_000);
});
