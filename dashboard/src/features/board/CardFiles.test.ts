// @vitest-environment jsdom
// The Files section of the card drawer: the list, the preview of markdown
// through the safe renderer, an upload of a name that exists as a new
// version, and the remove.
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";
import type { Card, CardFile } from "../../api/types";

const file = (name: string, version = 1, versions = 1): CardFile => ({ card: "P-1", name, version, size: 12, sha256: "x", media_type: "text/markdown; charset=utf-8", created_by: "operator", created_by_name: "operator", created_at: new Date().toISOString(), versions });
const list = vi.fn();
const upload = vi.fn();
const get = vi.fn();
const remove = vi.fn();
const versions = vi.fn();
vi.mock("../../stores", () => ({
  api: { cardFiles: { list, upload, get, remove, versions } },
  events: { on: () => () => {} },
}));

const { CardFiles } = await import("./CardFiles");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  vi.clearAllMocks();
});

const card = { id: "P-1", project: "p", archived: false } as unknown as Card;

function mount() {
  const el = document.createElement("div");
  document.body.append(el);
  dispose = render(() => createComponent(CardFiles, { card }), el);
  const button = (text: string) => [...el.querySelectorAll("button")].find((b) => b.textContent?.trim().startsWith(text));
  return { el, button };
}

describe("the Files section", () => {
  it("lists the files and renders a markdown preview safely", async () => {
    list.mockResolvedValue([file("spec.md", 2, 2)]);
    get.mockResolvedValue({ blob: new Blob(["# Title\n\n<img src=x onerror=alert(1)>"]), fileName: "spec.md", version: 2 });
    const { el, button } = mount();
    await waitFor(() => expect(button("spec.md")).toBeDefined());
    expect(el.textContent).toContain("v2 ·");
    expect(button("2 versions")).toBeDefined();
    button("spec.md")!.click();
    await waitFor(() => expect(el.querySelector(".prose h1")?.textContent).toBe("Title"));
    expect(el.querySelector(".prose [onerror]")).toBeNull();
    expect(get).toHaveBeenCalledWith("P-1", "spec.md");
  });

  it("uploads a name that exists as a new version", async () => {
    list.mockResolvedValue([file("spec.md")]);
    upload.mockResolvedValue({ ...file("spec.md", 2, 2), outcome: "replaced" });
    const { el, button } = mount();
    await waitFor(() => expect(button("spec.md")).toBeDefined());
    const input = el.querySelector<HTMLInputElement>('input[type="file"]')!;
    const f = new File(["# new"], "spec.md", { type: "text/markdown" });
    Object.defineProperty(input, "files", { value: [f], configurable: true });
    input.dispatchEvent(new Event("change"));
    await waitFor(() => expect(upload).toHaveBeenCalledWith("P-1", f, "spec.md", true));
    await waitFor(() => expect(el.textContent).toContain("v2 ·"));
  });

  it("refuses a type the card does not take, with no request", async () => {
    list.mockResolvedValue([]);
    const { el } = mount();
    await waitFor(() => expect(el.textContent).toContain("No files."));
    const input = el.querySelector<HTMLInputElement>('input[type="file"]')!;
    Object.defineProperty(input, "files", { value: [new File(["x"], "run.sh")], configurable: true });
    input.dispatchEvent(new Event("change"));
    await waitFor(() => expect(el.textContent).toContain("takes only"));
    expect(upload).not.toHaveBeenCalled();
  });

  it("removes a file after a confirm", async () => {
    list.mockResolvedValue([file("spec.md")]);
    remove.mockResolvedValue(undefined);
    const { el, button } = mount();
    await waitFor(() => expect(button("Remove")).toBeDefined());
    button("Remove")!.click();
    button("Yes, remove")!.click();
    await waitFor(() => expect(remove).toHaveBeenCalledWith("P-1", "spec.md"));
    await waitFor(() => expect(el.textContent).toContain("No files."));
  });
});
