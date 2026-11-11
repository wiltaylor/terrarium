import { expect, it } from "vitest";
import { highlightCode } from "./highlight";

it.each([
  ["sh", "echo $HOME"], ["css", "body { color: red; }"],
  ["js", "const x = 1;"], ["json", '{"x": true}'],
  ["py", "def f(): pass"], ["rs", "fn main() {}"],
  ["sql", "SELECT * FROM items"], ["ts", "const x: number = 1;"],
  ["html", "<p>text</p>"], ["yml", "key: true"],
])("highlights the bundled %s grammar or alias", (language, text) => {
  expect(highlightCode(text, language)).toContain('class="hljs-');
});

it("does not guess unknown languages, plain text or oversized code", () => {
  expect(highlightCode("const x = 1;", "unknown")).toBeUndefined();
  expect(highlightCode("const x = 1;", "")).toBeUndefined();
  expect(highlightCode("x".repeat(100_001), "js")).toBeUndefined();
});
