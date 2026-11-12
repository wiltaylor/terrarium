// @vitest-environment jsdom
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { SnapGrid } from "./SnapGrid";
import { autoHeight, placeWidget, readingOrder, snapDelta, type GridWidget } from "./snapGrid";

const widget = (id: string, x = 0, y = 0): GridWidget => ({ id, kind: "stat", x, y, w: 3, h: 2 });
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

it("snaps drags and resize deltas to whole cells, clamping to the grid", () => {
  expect(snapDelta(149, 100)).toBe(1);
  expect(snapDelta(151, 100)).toBe(2);
  const items = [widget("a")];
  expect(placeWidget(items, "a", { x: 20, y: -3, w: 7.3, h: 0 })[0]).toMatchObject({ x: 5, y: 0, w: 7, h: 1 });
  expect(items[0].x).toBe(0);
});

it("pushes overlapping widgets down in a deterministic cascade, preserving other columns", () => {
  const items = [widget("a", 0, 8), widget("b"), widget("c", 0, 2), widget("d", 6, 0)];
  expect(placeWidget(items, "a", { y: 0 }).map((item) => item.y)).toEqual([0, 2, 4, 0]);
  expect(placeWidget(items, "a", { y: 0 })).toEqual(placeWidget([...items].reverse(), "a", { y: 0 }).reverse());
});

it("uses reading order for the phone stack and content-only auto height defaults", () => {
  expect(readingOrder([widget("c", 0, 2), widget("b", 6), widget("a")]).map((item) => item.id)).toEqual(["a", "b", "c"]);
  expect(autoHeight({ ...widget("a"), kind: "markdown" })).toBe(true);
  expect(autoHeight({ ...widget("a"), kind: "wiki", auto_height: false })).toBe(false);
  expect(autoHeight({ ...widget("a"), auto_height: true })).toBe(false);
});

function mount(editing = true) {
  const [widgets, setWidgets] = createSignal([widget("a"), widget("b", 6)]);
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <SnapGrid widgets={widgets()} editing={editing} onChange={setWidgets} renderWidget={(item) => <p>{item.id}</p>} />, root);
  return { root, widgets, setWidgets };
}

it("moves with arrows and resizes with Shift+arrows, leaving read mode inert", () => {
  const { root, widgets } = mount();
  root.querySelector('button')!.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }));
  expect(widgets()[0].x).toBe(1);
  root.querySelector('button')!.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", shiftKey: true, bubbles: true }));
  expect(widgets()[0].h).toBe(3);
});

it("has no editing controls in read mode", () => {
  expect(mount(false).root.querySelectorAll("button")).toHaveLength(0);
});

it("retains exact cell positions across reload and equivalent live updates", () => {
  const { root, widgets, setWidgets } = mount(false);
  const positions = () => [...root.querySelectorAll<HTMLElement>("section")].map((item) => item.style.cssText);
  const initial = positions();
  setWidgets(JSON.parse(JSON.stringify(widgets())) as GridWidget[]);
  expect(positions()).toEqual(initial);
  dispose?.();
  dispose = render(() => <SnapGrid widgets={widgets()} renderWidget={(item) => <p>{item.id} live content</p>} />, root);
  expect(positions()).toEqual(initial);
});

it("expands content into whole rows and pushes lower widgets without changing saved cells", async () => {
  vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} });
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue({ height: 200 } as DOMRect);
  const items = [{ ...widget("a"), kind: "wiki" }, widget("b", 0, 2)];
  dispose = render(() => <SnapGrid widgets={items} renderWidget={(item) => <p>{item.id}</p>} />, document.body);
  await Promise.resolve();
  const sections = document.querySelectorAll<HTMLElement>("section");
  expect(sections[0].style.gridRow).toBe("1 / span 3");
  expect(sections[1].style.gridRow).toBe("4 / span 2");
  expect(items[1].y).toBe(2);
});
