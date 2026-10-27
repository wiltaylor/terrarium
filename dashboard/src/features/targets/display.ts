// The live view of a target display (docs/api.md §17.10): when to take the
// next screenshot, how a click on the shown image maps to screen pixels,
// and the label of a host's display state. No JSX; the tests drive it
// with fake timers.

import type { DisplayShot, Host, Target } from "../../api/types";

/** The time between two screenshots while the panel shows and is not paused. */
export const REFRESH_MS = 2500;
/** The longest wait after errors. */
export const MAX_BACKOFF_MS = 30_000;

export interface RefreshState {
  /** The panel is on the screen and the page is visible. */
  visible: boolean;
  paused: boolean;
  /** Screenshots that failed in a row. */
  errors: number;
}

/** The wait before the next screenshot, or `null` to stop. Each error in a row doubles the wait, up to 30 s. */
export function nextDelay(s: RefreshState): number | null {
  if (!s.visible || s.paused) return null;
  if (s.errors <= 0) return REFRESH_MS;
  return Math.min(MAX_BACKOFF_MS, REFRESH_MS * 2 ** Math.min(s.errors, 10));
}

/** The timer functions, so the tests can fake them. */
export interface Timers {
  set(fn: () => void, ms: number): unknown;
  clear(handle: unknown): void;
}

const realTimers: Timers = {
  set: (fn, ms) => setTimeout(fn, ms),
  clear: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
};

/**
 * A refresher: it calls `tick` at once when it starts, then after each
 * `nextDelay`. `tick` resolves true on a good screenshot, false on an
 * error. Hidden or paused, it stops; visible and resumed, it starts again.
 */
export function createRefresher(tick: () => Promise<boolean>, timers: Timers = realTimers) {
  const state: RefreshState = { visible: false, paused: false, errors: 0 };
  let handle: unknown = null;
  let running = false;
  let gen = 0;
  const stop = () => {
    if (handle !== null) timers.clear(handle);
    handle = null;
  };
  const schedule = (g: number) => {
    stop();
    const d = nextDelay(state);
    if (d === null || g !== gen) return;
    handle = timers.set(() => void run(g), d);
  };
  const run = async (g: number) => {
    handle = null;
    if (g !== gen || running || nextDelay(state) === null) return;
    running = true;
    let ok: boolean;
    try {
      ok = await tick();
    } catch {
      ok = false;
    }
    running = false;
    state.errors = ok ? 0 : state.errors + 1;
    schedule(g);
  };
  const restart = () => {
    gen += 1;
    stop();
    if (nextDelay(state) !== null) void run(gen);
  };
  return {
    state: () => ({ ...state }),
    setVisible(v: boolean) {
      if (state.visible === v) return;
      state.visible = v;
      restart();
    },
    setPaused(p: boolean) {
      if (state.paused === p) return;
      state.paused = p;
      restart();
    },
    /** Take a screenshot now (after an input operation). */
    now() {
      restart();
    },
    dispose() {
      gen += 1;
      stop();
      state.visible = false;
    },
  };
}

/** The part of a screenshot that maps a click: the shown size, the screen, the region. */
export type ShotGeometry = Pick<DisplayShot, "width" | "height" | "screen"> & { region?: DisplayShot["region"] };

/**
 * The screen pixel under a click on the shown image. `offset` is the click
 * position in the `<img>` box (CSS pixels from its top-left corner),
 * `rendered` the box size. The image shows the region (or the full
 * screen), scaled to the box.
 */
export function mapClick(offset: { x: number; y: number }, rendered: { width: number; height: number }, shot: ShotGeometry): { x: number; y: number } {
  const r = shot.region ?? { x: 0, y: 0, w: shot.screen.width, h: shot.screen.height };
  const shownW = Math.min(r.w, Math.max(0, shot.screen.width - r.x));
  const shownH = Math.min(r.h, Math.max(0, shot.screen.height - r.y));
  const fx = rendered.width > 0 ? offset.x / rendered.width : 0;
  const fy = rendered.height > 0 ? offset.y / rendered.height : 0;
  const clamp = (v: number, max: number) => Math.max(0, Math.min(max - 1, v));
  return {
    x: clamp(Math.round(r.x + fx * shownW), shot.screen.width),
    y: clamp(Math.round(r.y + fy * shownH), shot.screen.height),
  };
}

/** The `src` of the `<img>` for a screenshot. The CSP allows `data:`. */
export const shotSrc = (s: Pick<DisplayShot, "png_base64">): string => `data:image/png;base64,${s.png_base64}`;

/** Whether the host agent opened its own desktop (`[display] enabled = true`). */
export const hostDisplayOn = (h: Pick<Host, "capabilities" | "summary">): boolean => !!(h.capabilities?.display?.enabled ?? h.summary?.display);

/** The label of a host's display state. */
export const hostDisplayLabel = (h: Pick<Host, "capabilities" | "summary">): string => (hostDisplayOn(h) ? "display: on" : "display: off");

/** Whether the target has a screen (`has_display`, docs/api.md §17.10). */
export const targetHasDisplay = (t: Pick<Target, "has_display">): boolean => !!t.has_display;
