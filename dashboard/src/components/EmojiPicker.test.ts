// @vitest-environment jsdom
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { expect, it, vi } from "vitest";
import { EmojiPicker } from "./EmojiPicker";

it("searches GitHub names and selects a shortcode without submitting", () => {
  const host = document.createElement("div");
  document.body.append(host);
  const onPick = vi.fn();
  const dispose = render(() => createComponent(EmojiPicker, { onPick }), host);
  try {
    host.querySelector("button")!.click();
    const search = host.querySelector("input")!;
    search.value = "white_check_mark";
    search.dispatchEvent(new Event("input", { bubbles: true }));
    const pick = host.querySelector<HTMLButtonElement>('button[aria-label=":white_check_mark:"]')!;
    expect(pick.textContent).toBe("✅");
    expect(pick.type).toBe("button");
    pick.click();
    expect(onPick).toHaveBeenCalledWith(":white_check_mark:");
    expect(host.querySelector("input")).toBeNull();
    host.querySelector("button")!.click();
    host.querySelector("input")!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(host.querySelector("input")).toBeNull();
  } finally { dispose(); host.remove(); }
});
