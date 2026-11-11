// @vitest-environment jsdom
import { createSignal, For, Show } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { waitFor } from "../test/wait";

const visit = vi.fn();
vi.mock("../instance", () => ({ useInstance: () => ({ api: { readPositions: { visit } } }) }));
const { firstUnread, ReadBoundary, useReadBoundary } = await import("./ReadBoundary");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); window.location.hash = ""; vi.restoreAllMocks(); visit.mockReset(); });

it("compares timestamps, excluding an equal position and first visits", () => {
  expect(firstUnread(["2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z"], "2026-01-01T00:00:00.000Z")).toBe(1);
  expect(firstUnread(["2026-01-01T00:00:00Z"], null)).toBe(-1);
  expect(firstUnread([], "2026-01-01T00:00:00Z")).toBe(-1);
});

it.each(["thread", "mail", "card"] as const)("%s renders one boundary, scrolls to it, and records only the opening snapshot", async (kind) => {
  visit.mockResolvedValue({ previous: "2026-01-01T00:00:00Z", read_at: "2026-01-02T00:00:00Z" });
  const scroll = vi.fn();
  Object.defineProperty(HTMLElement.prototype, "scrollIntoView", { configurable: true, value: scroll });
  const [times, setTimes] = createSignal(["2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z"]);
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => {
    const read = useReadBoundary(kind, () => "item", times);
    return <For each={times()}>{(at, index) => <><Show when={index() === read.first()}><ReadBoundary /></Show><p>{at}</p></>}</For>;
  }, root);
  await waitFor(() => expect(root.querySelectorAll('[role="separator"]')).toHaveLength(1));
  expect(root.querySelector('[role="separator"]')?.nextElementSibling?.textContent).toBe("2026-01-02T00:00:00Z");
  expect(scroll).toHaveBeenCalledWith({ block: "start" });
  setTimes((old) => [...old, "2026-01-03T00:00:00Z"]);
  expect(visit).toHaveBeenCalledTimes(1);
  expect(visit).toHaveBeenCalledWith(kind, "item", "2026-01-02T00:00:00.000Z");
});

it("keeps an explicit permalink instead of scrolling", () => {
  const scroll = vi.fn();
  Object.defineProperty(HTMLElement.prototype, "scrollIntoView", { configurable: true, value: scroll });
  window.location.hash = "post-2";
  dispose = render(() => <ReadBoundary />, document.body);
  expect(scroll).not.toHaveBeenCalled();
});
