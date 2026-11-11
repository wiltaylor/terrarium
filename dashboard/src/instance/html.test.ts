// @vitest-environment jsdom
import { expect, it } from "vitest";
import { setSafeHtml } from "../lib/sanitize";
import { instanceHref } from "./create";
import { scopeHtmlLinks } from "./html";

it("scopes sanitized markdown links for normal and modified clicks without changing external links", () => {
  const el = document.createElement("div");
  setSafeHtml(el, '<a href="/p/demo/board?card=C1#reply">card</a><a href="/remote/lab/settings">settings</a><a href="https://example.org">external</a><a href="#reply">anchor</a><a href="javascript:alert(1)">bad</a>');
  scopeHtmlLinks(el, (path) => instanceHref("lab", path));
  expect(Array.from(el.querySelectorAll("a"), (a) => a.getAttribute("href"))).toEqual([
    "/remote/lab/p/demo/board?card=C1#reply", "/remote/lab/settings", "https://example.org", "#reply", null,
  ]);
});
