// @vitest-environment jsdom
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";
const capacity = vi.fn(async () => ({ percentage: 50, used: { cpu_millis: 1000, memory_bytes: 1024 }, reservable: { cpu_millis: 2000, memory_bytes: 2048 } }));
const reservations = vi.fn(async () => [{ id: "R1", holder: { id: "worker" }, state: "active", exclusive: true, start: 1000, until: 3000, reason: "build" }]);
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores(({ api: { hosts: { capacity, reservations } } })); });
const { HostReservations, reservationProgress } = await import("./Reservations");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.innerHTML = ""; vi.clearAllMocks(); });
it("clamps elapsed timeline progress and handles queued grants", () => {
  expect(reservationProgress(null, null, 100)).toBe(0);
  expect(reservationProgress(1000, 3000, 2000)).toBe(50);
  expect(reservationProgress(1000, 3000, 4000)).toBe(100);
  expect(reservationProgress(1000, 3000, 0)).toBe(0);
});
it("shows capacity bars and the holder timeline", async () => {
  const el = document.createElement("div"); document.body.append(el);
  dispose = render(() => createComponent(HostReservations, { host: "h", supported: true, now: 2000 }), el);
  await waitFor(() => expect(el.textContent).toContain("worker"));
  expect(el.textContent).toContain("50%");
  expect(el.querySelector<HTMLProgressElement>('progress[aria-label="CPU reserved capacity"]')?.value).toBe(1000);
  expect(el.querySelector<HTMLProgressElement>('progress[aria-label="Reservation R1 elapsed"]')?.value).toBe(50);
});
it("does not request unsupported host resources", async () => {
  const el = document.createElement("div");
  dispose = render(() => createComponent(HostReservations, { host: "old", supported: false, now: 2000 }), el);
  await Promise.resolve();
  expect(capacity).not.toHaveBeenCalled();
  expect(el.textContent).toBe("");
});
