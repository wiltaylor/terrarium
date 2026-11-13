import { createEffect, createMemo, createSignal, For, onCleanup, onMount, Show, type JSX } from "solid-js";
import { autoHeight, GRID_GAP, placeWidget, readingOrder, ROW_HEIGHT, snapDelta, type GridWidget } from "./snapGrid";
import "./SnapGrid.css";

export interface SnapGridProps<T extends GridWidget> {
  widgets: readonly T[];
  editing?: boolean;
  /** Project pages v2 only. Legacy consumers keep their fixed/scrolling cells. */
  documentFlow?: boolean;
  onChange?: (widgets: T[]) => void;
  renderWidget: (widget: T) => JSX.Element;
  /** Reactive content kept mounted when only a widget's cells or source change. */
  renderLiveWidget?: (widget: () => T) => JSX.Element;
}

/** Controlled layout: live data can change without copying or resetting a draft. */
export function SnapGrid<T extends GridWidget>(props: SnapGridProps<T>) {
  let grid!: HTMLDivElement;
  const [heights, setHeights] = createSignal<Record<string, number>>({});
  const layout = createMemo(() => {
    const widgets = props.widgets.map((widget) => (props.documentFlow || autoHeight(widget)) && heights()[widget.id]
      ? { ...widget, h: Math.max(widget.h, heights()[widget.id]!) } : widget);
    return readingOrder(placeWidget(widgets, readingOrder(widgets)[0]?.id ?? "", {}));
  });
  function measure(element: HTMLDivElement, current: () => T) {
    const update = () => {
      const widget = current();
      if (!widget) return;
      if (!props.documentFlow && !autoHeight(widget)) return;
      const height = Math.max(element.getBoundingClientRect().height, props.documentFlow && widget.frame === "viewport" ? window.innerHeight : 0);
      const rows = Math.max(1, Math.ceil((height + (props.editing ? 32 : 0) + GRID_GAP) / (ROW_HEIGHT + GRID_GAP)));
      setHeights((old) => old[widget.id] === rows ? old : { ...old, [widget.id]: rows });
    };
    onMount(() => {
      update();
      const observer = typeof ResizeObserver === "undefined" ? undefined : new ResizeObserver(update);
      observer?.observe(element);
      window.addEventListener("resize", update);
      onCleanup(() => { observer?.disconnect(); window.removeEventListener("resize", update); });
    });
    createEffect(update);
  }
  let gesture: { id: string; resize: boolean; x: number; y: number; widgets: readonly T[] } | undefined;
  function change(widgets: readonly T[], id: string, patch: Partial<Pick<GridWidget, "x" | "y" | "w" | "h">>, resize: boolean) {
    const framed = widgets.find(item => item.id === id)?.frame;
    const boundedPatch = props.documentFlow && framed && framed !== "grid" ? { ...patch, x: 0, w: 12 } : patch;
    props.onChange?.(placeWidget(widgets, id, boundedPatch).map((item) =>
      resize && item.id === id && autoHeight(item) ? { ...item, auto_height: false } : item));
  }
  function start(event: PointerEvent, id: string, resize: boolean) {
    if (!props.editing || event.button !== 0) return;
    event.preventDefault();
    (event.currentTarget as HTMLElement).setPointerCapture(event.pointerId);
    gesture = { id, resize, x: event.clientX, y: event.clientY, widgets: props.widgets };
  }
  function finish(event: PointerEvent) {
    if (!gesture) return;
    const active = gesture;
    gesture = undefined;
    if (!props.editing) return;
    const widget = active.widgets.find((item) => item.id === active.id)!;
    const dx = snapDelta(event.clientX - active.x, (grid.getBoundingClientRect().width + GRID_GAP) / 12);
    const dy = snapDelta(event.clientY - active.y, ROW_HEIGHT + GRID_GAP);
    change(active.widgets, active.id, active.resize
      ? { w: widget.w + dx, h: widget.h + dy }
      : { x: widget.x + dx, y: widget.y + dy }, active.resize);
  }
  function key(event: KeyboardEvent, widget: T) {
    if (!props.editing) return;
    const delta = ({ ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] } as Record<string, readonly [number, number]>)[event.key];
    if (!delta) return;
    event.preventDefault();
    change(props.widgets, widget.id, event.shiftKey
      ? { w: widget.w + delta[0], h: widget.h + delta[1] }
      : { x: widget.x + delta[0], y: widget.y + delta[1] }, event.shiftKey);
  }
  return <div ref={grid} class="snap-grid" classList={{ "snap-grid-document": props.documentFlow }} aria-label="Widget layout">
    <For each={layout().map((widget) => widget.id)}>{(id) => {
      const widget = () => layout().find((item) => item.id === id)!;
      return <section class="snap-grid-item" data-widget-id={id} data-auto={autoHeight(widget())} data-frame={props.documentFlow ? widget().frame ?? "grid" : undefined}
      style={{ "grid-column": `${widget().x + 1} / span ${widget().w}`, "grid-row": `${widget().y + 1} / span ${widget().h}`, "--widget-height": `${widget().h * (ROW_HEIGHT + GRID_GAP) - GRID_GAP}px` }}>
      <Show when={props.editing}><div class="snap-grid-controls">
        <button type="button" aria-label={`Move ${id}`} onKeyDown={(event) => key(event, widget())} onPointerDown={(event) => start(event, id, false)} onPointerUp={finish} onPointerCancel={() => { gesture = undefined; }}>Move</button>
        <button type="button" aria-label={`Resize ${id}`} onKeyDown={(event) => key(event, widget())} onPointerDown={(event) => start(event, id, true)} onPointerUp={finish} onPointerCancel={() => { gesture = undefined; }}>Resize</button>
      </div></Show>
      <div class="snap-grid-content"><div ref={(element) => measure(element, () => props.widgets.find(item => item.id === id)!)}>{props.renderLiveWidget ? props.renderLiveWidget(widget) : props.renderWidget(widget())}</div></div>
    </section>;
    }}</For>
  </div>;
}
