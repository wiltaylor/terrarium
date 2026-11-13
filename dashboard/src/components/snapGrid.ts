export interface GridWidget {
  id: string;
  kind: string;
  x: number;
  y: number;
  w: number;
  h: number;
  auto_height?: boolean;
  frame?: "grid" | "full_width" | "viewport";
}

export const ROW_HEIGHT = 64;
export const GRID_GAP = 8;
export const autoHeight = (widget: GridWidget) =>
  (widget.kind === "markdown" || widget.kind === "wiki") && widget.auto_height !== false;

export function readingOrder<T extends GridWidget>(widgets: readonly T[]): T[] {
  return [...widgets].sort((a, b) => a.y - b.y || a.x - b.x || a.id.localeCompare(b.id));
}

/** The edited item has priority. Displaced items keep their horizontal cells. */
export function placeWidget<T extends GridWidget>(widgets: readonly T[], id: string, patch: Partial<Pick<GridWidget, "x" | "y" | "w" | "h">>): T[] {
  const result = widgets.map((widget) => {
    const next = { ...widget, ...(widget.id === id ? patch : {}) };
    next.w = Math.max(1, Math.min(12, Math.round(next.w)));
    next.h = Math.max(1, Math.round(next.h));
    next.x = Math.max(0, Math.min(12 - next.w, Math.round(next.x)));
    next.y = Math.max(0, Math.round(next.y));
    return next;
  });
  const edited = result.find((widget) => widget.id === id);
  const placed: T[] = edited ? [edited] : [];
  for (const widget of readingOrder(result.filter((item) => item !== edited))) {
    let collision: T | undefined;
    while ((collision = placed.find((other) => widget.x < other.x + other.w && widget.x + widget.w > other.x && widget.y < other.y + other.h && widget.y + widget.h > other.y))) {
      widget.y = collision.y + collision.h;
    }
    placed.push(widget);
  }
  return result;
}

export function snapDelta(pixels: number, cell: number): number {
  return cell > 0 ? Math.round(pixels / cell) : 0;
}
