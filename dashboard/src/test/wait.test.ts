import { expect, it, vi } from "vitest";
import { waitFor } from "./wait";

it("uses the standard polling interval without shortening the load timeout", async () => {
  const spy = vi.spyOn(vi, "waitFor");
  try {
    await waitFor(() => true);
    expect(spy).toHaveBeenCalledWith(expect.any(Function), { timeout: 10_000, interval: 50 });
  } finally {
    spy.mockRestore();
  }
});
