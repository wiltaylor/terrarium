// @vitest-environment jsdom
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { expect, it } from "vitest";
import { EtaAccuracy } from "./EtaAccuracy";

it("shows raw and calibrated accuracy with distinct sample counts and backtest caveat", () => {
  const host = document.createElement("div");
  const dispose = render(() => createComponent(EtaAccuracy, { value: {
    raw: { samples: 30, on_time_pct: 50, within_half_to_double_pct: 20 },
    calibrated: { samples: 25, on_time_pct: 68, within_half_to_double_pct: 72 },
  } }), host);
  try {
    expect(host.querySelector('[aria-label="ETA accuracy"]')).not.toBeNull();
    for (const text of ["Raw worker guesses", "Calibrated backtest", "30 samples", "25 samples", "On time: 50%", "On time: 68%", "Within half to double: 72%", "before each guess", "2,000 project card moves"]) {
      expect(host.textContent).toContain(text);
    }
  } finally { dispose(); }
});

it("shows empty states instead of misleading zero percentages", () => {
  const host = document.createElement("div");
  const empty = { samples: 0, on_time_pct: null, within_half_to_double_pct: null };
  const dispose = render(() => createComponent(EtaAccuracy, { value: { raw: empty, calibrated: empty } }), host);
  try {
    expect(host.textContent).toContain("No completed estimates with enough history yet.");
    expect(host.textContent).not.toContain("On time:");
  } finally { dispose(); }
});
