// @vitest-environment jsdom
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
const preferences = vi.fn(async () => ({ default_agent: "meta/lead" }));
const setPreferences = vi.fn(async (body) => body);
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores(({
  api: { account: { preferences, setPreferences } },
  agents: { state: { agents: [{ project: "meta", name: "lead" }, { project: "demo", name: "dev" }] } },
})); });
const { DefaultAgent } = await import("./DefaultAgent");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.innerHTML = ""; });
it("loads and saves the server preference and clears it", async () => {
  const el = document.createElement("div"); document.body.append(el);
  dispose = render(() => createComponent(DefaultAgent, {}), el);
  await new Promise((r) => setTimeout(r, 0));
  const select = document.querySelector("select")!;
  expect(select.value).toBe("meta/lead");
  select.value = "demo/dev"; select.dispatchEvent(new Event("change", { bubbles: true }));
  await new Promise((r) => setTimeout(r, 0));
  expect(setPreferences).toHaveBeenLastCalledWith({ default_agent: "demo/dev" });
  select.value = ""; select.dispatchEvent(new Event("change", { bubbles: true }));
  await new Promise((r) => setTimeout(r, 0));
  expect(setPreferences).toHaveBeenLastCalledWith({ default_agent: null });
});
