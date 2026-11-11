// @vitest-environment jsdom
// The Links tab (docs/api.md §36): it lists the links at once, lists again
// after a new link, and Show revoked turns the revoked rows on and off.
import { render } from "solid-js/web";
import { createComponent } from "solid-js";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, autoLoginMs: 0 });
vi.mock("../../stores", () => ({ api }));

const { default: LinksPage } = await import("./LinksPage");

const tick = () => new Promise((r) => setTimeout(r, 0));
async function settle() {
  for (let i = 0; i < 5; i++) await tick();
}

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
  document.body.replaceChildren();
});

function mount() {
  const root = document.createElement("div");
  document.body.append(root);
  dispose = render(() => createComponent(LinksPage, {}), root);
}

const rows = () => [...document.querySelectorAll("tbody tr")].map((r) => r.textContent ?? "");
const button = (label: string) => [...document.querySelectorAll("button")].find((b) => b.textContent?.trim() === label)!;

describe("LinksPage", () => {
  it("lists the links on open, after a new link, and with and without the revoked ones", async () => {
    const list = vi.spyOn(api.links, "list");
    const a = await api.links.create("office");
    const b = await api.links.create("old");
    await api.links.revoke(b.link.id);
    mount();
    await settle();
    expect(list).toHaveBeenCalledWith({ revoked: false });
    expect(rows().some((r) => r.includes("office"))).toBe(true);
    expect(rows().some((r) => r.includes("old"))).toBe(false);

    // A new link through the dialog: the list loads again when it closes.
    button("New link").click();
    await settle();
    const input = document.querySelector<HTMLInputElement>("input[placeholder='office']")!;
    input.value = "lab";
    input.dispatchEvent(new Event("input", { bubbles: true }));
    document.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await settle();
    expect(document.body.textContent).toContain("tlp_");
    button("Done").click();
    await settle();
    expect(rows().some((r) => r.includes("lab"))).toBe(true);

    // Show revoked on, then off again.
    const toggle = document.querySelector<HTMLInputElement>("input[type='checkbox']")!;
    toggle.click();
    await settle();
    expect(list).toHaveBeenLastCalledWith({ revoked: true });
    expect(rows().some((r) => r.includes("old"))).toBe(true);
    toggle.click();
    await settle();
    expect(list).toHaveBeenLastCalledWith({ revoked: false });
    expect(rows().some((r) => r.includes("old"))).toBe(false);
    expect(rows().some((r) => r.includes(a.link.label))).toBe(true);
  });
});
