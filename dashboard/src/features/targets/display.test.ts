import { describe, expect, it } from "vitest";
import { createRefresher, hostDisplayLabel, hostDisplayOn, mapClick, MAX_BACKOFF_MS, nextDelay, REFRESH_MS, shotSrc, type Timers } from "./display";

/** Timers that run only when the test says so. */
function fakeTimers() {
  const queue: { fn: () => void; ms: number; id: number }[] = [];
  let id = 0;
  const t: Timers = {
    set: (fn, ms) => {
      queue.push({ fn, ms, id: ++id });
      return id;
    },
    clear: (h) => {
      const i = queue.findIndex((q) => q.id === h);
      if (i >= 0) queue.splice(i, 1);
    },
  };
  return {
    t,
    pending: () => queue.map((q) => q.ms),
    fire: async () => {
      const q = queue.shift();
      q?.fn();
      await new Promise((r) => setTimeout(r, 0));
    },
  };
}
const flush = () => new Promise((r) => setTimeout(r, 0));

describe("nextDelay", () => {
  it("refreshes every 2.5 s while visible, and stops when hidden or paused", () => {
    expect(nextDelay({ visible: true, paused: false, errors: 0 })).toBe(REFRESH_MS);
    expect(nextDelay({ visible: false, paused: false, errors: 0 })).toBeNull();
    expect(nextDelay({ visible: true, paused: true, errors: 0 })).toBeNull();
  });

  it("doubles the wait after each error, up to 30 s", () => {
    expect(nextDelay({ visible: true, paused: false, errors: 1 })).toBe(5000);
    expect(nextDelay({ visible: true, paused: false, errors: 2 })).toBe(10_000);
    expect(nextDelay({ visible: true, paused: false, errors: 9 })).toBe(MAX_BACKOFF_MS);
  });
});

describe("createRefresher", () => {
  it("ticks at once when visible, then on the interval; stops when paused", async () => {
    const ft = fakeTimers();
    let calls = 0;
    const r = createRefresher(async () => (++calls, true), ft.t);
    expect(calls).toBe(0);
    r.setVisible(true);
    await flush();
    expect(calls).toBe(1);
    expect(ft.pending()).toEqual([REFRESH_MS]);
    await ft.fire();
    expect(calls).toBe(2);
    r.setPaused(true);
    expect(ft.pending()).toEqual([]);
    r.setPaused(false);
    await flush();
    expect(calls).toBe(3);
    r.dispose();
    expect(ft.pending()).toEqual([]);
  });

  it("backs off after errors and resets after a good screenshot", async () => {
    const ft = fakeTimers();
    let ok = false;
    const r = createRefresher(async () => ok, ft.t);
    r.setVisible(true);
    await flush();
    expect(ft.pending()).toEqual([5000]);
    await ft.fire();
    expect(ft.pending()).toEqual([10_000]);
    ok = true;
    await ft.fire();
    expect(r.state().errors).toBe(0);
    expect(ft.pending()).toEqual([REFRESH_MS]);
  });

  it("stops when the page hides", async () => {
    const ft = fakeTimers();
    const r = createRefresher(async () => true, ft.t);
    r.setVisible(true);
    await flush();
    r.setVisible(false);
    expect(ft.pending()).toEqual([]);
  });
});

describe("mapClick", () => {
  const screen = { width: 1280, height: 800 };
  it("maps a click on a scaled full screen", () => {
    const shot = { width: 640, height: 400, screen };
    expect(mapClick({ x: 320, y: 200 }, { width: 640, height: 400 }, shot)).toEqual({ x: 640, y: 400 });
    // The <img> shows at 320x200 CSS pixels.
    expect(mapClick({ x: 80, y: 50 }, { width: 320, height: 200 }, shot)).toEqual({ x: 320, y: 200 });
  });

  it("adds the region offset", () => {
    const shot = { width: 200, height: 100, screen, region: { x: 100, y: 50, w: 400, h: 200 } };
    expect(mapClick({ x: 100, y: 50 }, { width: 200, height: 100 }, shot)).toEqual({ x: 300, y: 150 });
  });

  it("keeps the point on the screen", () => {
    const shot = { width: 640, height: 400, screen };
    expect(mapClick({ x: 640, y: 400 }, { width: 640, height: 400 }, shot)).toEqual({ x: 1279, y: 799 });
    expect(mapClick({ x: -5, y: -5 }, { width: 640, height: 400 }, shot)).toEqual({ x: 0, y: 0 });
  });
});

describe("labels", () => {
  it("names the host display state", () => {
    expect(hostDisplayLabel({ capabilities: { display: { enabled: true } } })).toBe("display: on");
    expect(hostDisplayLabel({ capabilities: {} })).toBe("display: off");
    expect(hostDisplayOn({ summary: { os: "linux", arch: "x86_64", docker: false, vmlab: false, display: true } })).toBe(true);
  });

  it("makes a data URL", () => {
    expect(shotSrc({ png_base64: "AAA" })).toBe("data:image/png;base64,AAA");
  });
});
