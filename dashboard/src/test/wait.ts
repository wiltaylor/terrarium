// Waits for tests that do not depend on how fast the machine is. A fixed
// `setTimeout(r, 20)` races the mock's own timers: on a loaded machine the
// test's timer can fire before the mock's next step, and the test fails.
import { onTestFinished, vi } from "vitest";

/**
 * Runs `check` until it does not throw (an `expect` in it passes), then gives
 * its value. Use it for "this event comes": it waits only as long as needed.
 * Use it, not a bare `vi.waitFor`: the 1 s default of `vi.waitFor` fails on a
 * loaded merge host (TER-707). The 10 s default only slows a test that fails.
 */
export function waitFor<T>(check: () => T | Promise<T>, timeout = 10_000): Promise<T> {
  return vi.waitFor(check, { timeout, interval: 50 });
}

/**
 * Fakes `setTimeout` and `setInterval` for the rest of the test (`Date` and
 * microtasks stay real). Gives `tick(ms)`: it moves the fake clock by `ms`,
 * runs each timer that is due in order, and lets the promises they start
 * settle. Use it when the mock waits on purpose (a run that takes 20 ms) or
 * when the test checks that something has *not* happened yet.
 */
export function fakeClock(): (ms?: number) => Promise<void> {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval"] });
  onTestFinished(() => void vi.useRealTimers());
  return async (ms = 0) => {
    await vi.advanceTimersByTimeAsync(ms);
  };
}
