import { createMemo, createSignal, For, onCleanup, onMount, Show, type JSX } from "solid-js";
import { autoHeight, GRID_GAP, placeWidget, readingOrder, ROW_HEIGHT, snapDelta, type GridWidget } from "./snapGrid";
import "./SnapGrid.css";

export interface SnapGridProps<T extends GridWidget> {
  widgets: readonly T[];
  editing?: boolean;
  onChange?: (widgets: T[]) => void;
  renderWidget: (widget: T) => JSX.Element;
}

/** Controlled layout: live data can change without copying or resetting a draft. */
export function SnapGrid<T extends GridWidget>(props: SnapGridProps<T>) {
  let grid!: HTMLDivElement;
  const [heights, setHeights] = createSignal<Record<string, number>>({});
  const layout = createMemo(() => {
    const widgets = props.widgets.map((widget) => autoHeight(widget) && heights()[widget.id]
      ? { ...widget, h: Math.max(widget.h, heights()[widget.id]!) } : widget);
    return readingOrder(placeWidget(widgets, readingOrder(widgets)[0]?.id ?? "", {}));
  });
  function measure(element: HTMLDivElement, widget: T) {
    if (!autoHeight(widget)) return;
    const update = () => {
      const rows = Math.max(1, Math.ceil((element.getBoundingClientRect().height + (props.editing ? 32 : 0) + GRID_GAP) / (ROW_HEIGHT + GRID_GAP)));
      setHeights((old) => old[widget.id] === rows ? old : { ...old, [widget.id]: rows });
    };
    onMount(() => {
      update();
      const observer = new ResizeObserver(update);
      observer.observe(element);
      onCleanup(() => observer.disconnect());
    });
  }
  let gesture: { id: string; resize: boolean; x: number; y: number; widgets: readonly T[] } | undefined;
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
    props.onChange?.(placeWidget(active.widgets, active.id, active.resize
      ? { w: widget.w + dx, h: widget.h + dy }
      : { x: widget.x + dx, y: widget.y + dy }));
  }
  function key(event: KeyboardEvent, widget: T) {
    if (!props.editing) return;
    const delta = ({ ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] } as Record<string, readonly [number, number]>)[event.key];
    if (!delta) return;
    event.preventDefault();
    props.onChange?.(placeWidget(props.widgets, widget.id, event.shiftKey
      ? { w: widget.w + delta[0], h: widget.h + delta[1] }
      : { x: widget.x + delta[0], y: widget.y + delta[1] }));
  }
  return <div ref={grid} class="snap-grid" aria-label="Widget layout">
    <For each={layout().map((widget) => widget.id)}>{(id) => {
      const widget = () => layout().find((item) => item.id === id)!;
      return <section class="snap-grid-item" data-widget-id={id} data-auto={autoHeight(widget())}
      style={{ "grid-column": `${widget().x + 1} / span ${widget().w}`, "grid-row": `${widget().y + 1} / span ${widget().h}`, "--widget-height": `${widget().h * (ROW_HEIGHT + GRID_GAP) - GRID_GAP}px` }}>
      <Show when={props.editing}><div class="snap-grid-controls">
        <button type="button" aria-label={`Move ${id}`} onKeyDown={(event) => key(event, widget())} onPointerDown={(event) => start(event, id, false)} onPointerUp={finish} onPointerCancel={() => { gesture = undefined; }}>Move</button>
        <button type="button" aria-label={`Resize ${id}`} onKeyDown={(event) => key(event, widget())} onPointerDown={(event) => start(event, id, true)} onPointerUp={finish} onPointerCancel={() => { gesture = undefined; }}>Resize</button>
      </div></Show>
      <div class="snap-grid-content"><div ref={(element) => measure(element, widget())}>{props.renderWidget(widget())}</div></div>
    </section>;
    }}</For>
  </div>;
}
