// @vitest-environment jsdom
// The on-screen keys of the agent terminal (issue #12): the key names they
// send, the sticky modifiers, the rate limit, and a refused send.
import { createComponent, createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { waitFor } from "../test/wait";
import { ApiError } from "../api/http";
import { KeyPad } from "./KeyPad";
import { KeyRate, type Modifier } from "./terminalKeys";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
});

function mount(send: (keys: string[]) => Promise<unknown>, opts: { enabled?: boolean; rate?: KeyRate } = {}) {
  const el = document.createElement("div");
  document.body.append(el);
  const [mods, setMods] = createSignal<ReadonlySet<Modifier>>(new Set());
  dispose = render(
    () =>
      createComponent(KeyPad, {
        send,
        get enabled() {
          return opts.enabled ?? true;
        },
        get mods() {
          return mods();
        },
        onToggle: (m: Modifier) =>
          setMods((cur) => {
            const next = new Set(cur);
            if (next.has(m)) next.delete(m);
            else next.add(m);
            return next;
          }),
        onUsed: () => setMods(new Set<Modifier>()),
        rate: opts.rate ?? new KeyRate(),
      }),
    el,
  );
  const button = (name: string) => el.querySelector<HTMLButtonElement>(`button[aria-label="${name}"]`) ?? [...el.querySelectorAll("button")].find((b) => b.textContent === name)!;
  return { el, button, mods };
}

describe("KeyPad", () => {
  it("sends the key names; a sticky modifier applies to the next key only", async () => {
    const send = vi.fn().mockResolvedValue({ accepted: true });
    const { button, mods } = mount(send);
    button("Escape").click();
    expect(send).toHaveBeenLastCalledWith(["esc"]);
    const ctrl = button("Ctrl");
    ctrl.click();
    expect(ctrl.getAttribute("aria-pressed")).toBe("true");
    button("Shift").click();
    button("Up arrow").click();
    expect(send).toHaveBeenLastCalledWith(["ctrl-shift-up"]);
    expect(mods().size).toBe(0);
    expect(ctrl.getAttribute("aria-pressed")).toBe("false");
    button("Shift").click();
    button("Tab").click();
    expect(send).toHaveBeenLastCalledWith(["shift-tab"]);
    // Ctrl-C is a chord already: the modifiers do not apply, and stay armed.
    button("Alt").click();
    button("Control C").click();
    expect(send).toHaveBeenLastCalledWith(["ctrl-c"]);
    expect(mods().has("alt")).toBe(true);
  });

  it("keeps the focus: a press does not take it from the terminal", () => {
    const { button } = mount(vi.fn().mockResolvedValue({}));
    const down = new Event("pointerdown", { bubbles: true, cancelable: true });
    button("Escape").dispatchEvent(down);
    expect(down.defaultPrevented).toBe(true);
  });

  it("is off while input is off", () => {
    const send = vi.fn();
    const { el, button } = mount(send, { enabled: false });
    expect([...el.querySelectorAll("button")].every((b) => b.disabled)).toBe(true);
    button("Escape").click();
    expect(send).not.toHaveBeenCalled();
  });

  it("keeps to the rate limit, and shows a refused send", async () => {
    const send = vi.fn().mockRejectedValue(new ApiError(429, "more than 20 keys in one second", undefined, "rate_limited"));
    const { el, button } = mount(send, { rate: new KeyRate(1) });
    button("Escape").click();
    await waitFor(() => expect(el.querySelector('[role="status"]')?.textContent).toMatch(/Too many keys/));
    send.mockClear();
    button("Escape").click();
    expect(send).not.toHaveBeenCalled();
  });
});
