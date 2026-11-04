import { describe, expect, it } from "vitest";
import { chord, KEYPAD, KeyRate, KEYS_PER_SEC, typedKeys, type Modifier } from "./terminalKeys";

const mods = (...m: Modifier[]) => new Set<Modifier>(m);

describe("the on-screen keys (issue #12)", () => {
  it("has the keys a phone keyboard lacks, in the daemon's key names", () => {
    expect(KEYPAD.map((k) => k.key)).toEqual(["esc", "tab", "up", "down", "left", "right", "home", "end", "pageup", "pagedown", "enter", "ctrl-c"]);
    // Every key has an accessible name.
    expect(KEYPAD.every((k) => k.title.length > 0)).toBe(true);
  });

  it("puts the armed modifiers in front, in the canonical order", () => {
    expect(chord("esc", mods())).toBe("esc");
    expect(chord("tab", mods("shift"))).toBe("shift-tab");
    expect(chord("up", mods("shift", "ctrl"))).toBe("ctrl-shift-up");
    expect(chord("left", mods("alt", "shift", "ctrl"))).toBe("ctrl-alt-shift-left");
    // A trailing `-` stays a key: the daemon reads `ctrl--` as Ctrl and minus.
    expect(chord("-", mods("ctrl"))).toBe("ctrl--");
  });

  it("sends a typed character as a chord while a modifier is armed", () => {
    expect(typedKeys("c", mods())).toBeNull();
    expect(typedKeys("c", mods("ctrl"))).toEqual(["ctrl-c"]);
    expect(typedKeys("C", mods("ctrl"))).toEqual(["ctrl-shift-c"]);
    expect(typedKeys("b", mods("alt"))).toEqual(["alt-b"]);
    expect(typedKeys("7", mods("ctrl"))).toEqual(["ctrl-7"]);
    expect(typedKeys(" ", mods("ctrl"))).toEqual(["ctrl-space"]);
    expect(typedKeys("[", mods("ctrl"))).toEqual(["ctrl-["]);
    // Only the first character takes the modifier; the rest is text.
    expect(typedKeys("dex", mods("ctrl"))).toEqual(["ctrl-d", "text:ex"]);
    // No key name: the text goes as typed.
    expect(typedKeys("é", mods("ctrl"))).toBeNull();
    expect(typedKeys("\x1b[A", mods("ctrl"))).toBeNull();
    expect(typedKeys("", mods("ctrl"))).toBeNull();
  });

  it("keeps to the daemon's 20 keys in one second", () => {
    let t = 0;
    const rate = new KeyRate(KEYS_PER_SEC, () => t);
    expect(rate.take(15)).toBe(true);
    expect(rate.take(5)).toBe(true);
    expect(rate.take(1)).toBe(false);
    t = 500;
    expect(rate.take(1)).toBe(false);
    // A refused take takes nothing.
    t = 1000;
    expect(rate.take(20)).toBe(true);
    expect(rate.take(1)).toBe(false);
  });
});
