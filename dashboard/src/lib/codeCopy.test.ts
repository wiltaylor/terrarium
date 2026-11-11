// @vitest-environment jsdom
import { afterEach, expect, it, vi } from "vitest";
import { mountCodeCopy } from "./codeCopy";
import { renderMarkdown } from "./markdown";
import { setSafeHtml } from "./sanitize";

afterEach(() => vi.unstubAllGlobals());

it("copies raw highlighted text, not markup or controls, after sanitizing", async () => {
  const writeText = vi.fn().mockResolvedValue(undefined);
  vi.stubGlobal("navigator", { clipboard: { writeText } });
  const root = document.createElement("div");
  const text = 'const x = "<img onerror=alert(1)>";';
  setSafeHtml(root, renderMarkdown(`\`\`\`js\n${text}\n\`\`\``));
  mountCodeCopy(root);
  expect(root.querySelector(".hljs-keyword")?.textContent).toBe("const");
  expect(root.querySelector("img")).toBeNull();
  const button = root.querySelector("button")!;
  expect(button.type).toBe("button");
  button.click();
  await vi.waitFor(() => expect(button.textContent).toBe("Copied"));
  expect(writeText).toHaveBeenCalledWith(text);
});

it("reports clipboard failure and allows retry", async () => {
  const writeText = vi.fn().mockRejectedValueOnce(new Error("denied")).mockResolvedValue(undefined);
  vi.stubGlobal("navigator", { clipboard: { writeText } });
  const root = document.createElement("div");
  setSafeHtml(root, renderMarkdown("```unknown\nx & y\n```\n\n`inline`"));
  mountCodeCopy(root);
  expect(root.querySelectorAll("button")).toHaveLength(1);
  const button = root.querySelector("button")!;
  button.click();
  await vi.waitFor(() => expect(button.textContent).toContain("failed"));
  button.click();
  await vi.waitFor(() => expect(button.textContent).toBe("Copied"));
  expect(writeText).toHaveBeenLastCalledWith("x & y");
});

it("does not retain controls when Markdown is replaced", () => {
  const root = document.createElement("div");
  setSafeHtml(root, renderMarkdown("```\none\n```"));
  mountCodeCopy(root);
  expect(root.querySelectorAll("button")).toHaveLength(1);
  setSafeHtml(root, renderMarkdown("new body"));
  mountCodeCopy(root);
  expect(root.querySelector("button")).toBeNull();
});
