// The on-screen keys of the agent terminal (issue #12), with no UI: the
// keys that a phone keyboard has not (Esc, Tab, Ctrl, Alt, Shift, the
// arrows, Home/End, PgUp/PgDn). They go to `POST /api/agents/{id}/keys`
// as the chord names of the daemon (`TerminalKey`, docs/api.md §6.5):
// `esc`, `ctrl-c`, `shift-tab`, `alt-up`, `text:<text>`.
//
// - `KEYPAD`: the key buttons, in screen order.
// - `chord`: a key name with the sticky modifiers in front.
// - `typedKeys`: a typed text while a modifier is armed, as key names.
// - `KeyRate`: the client side of the daemon's rate limit.

/** A sticky modifier: it applies to the next key or typed character. */
export type Modifier = "ctrl" | "alt" | "shift";

/** The modifiers in the order of the daemon's canonical chord (`ctrl-alt-shift-x`). */
export const MODIFIERS: readonly Modifier[] = ["ctrl", "alt", "shift"];

export interface PadKey {
  /** The text on the button. */
  label: string;
  /** The daemon's key name. */
  key: string;
  /** The accessible name, when the label is a symbol. */
  title: string;
  /** True: the sticky modifiers do not apply (Ctrl-C is a chord already). */
  fixed?: boolean;
}

/** The key buttons, in screen order. */
export const KEYPAD: readonly PadKey[] = [
  { label: "Esc", key: "esc", title: "Escape" },
  { label: "Tab", key: "tab", title: "Tab" },
  { label: "↑", key: "up", title: "Up arrow" },
  { label: "↓", key: "down", title: "Down arrow" },
  { label: "←", key: "left", title: "Left arrow" },
  { label: "→", key: "right", title: "Right arrow" },
  { label: "Home", key: "home", title: "Home" },
  { label: "End", key: "end", title: "End" },
  { label: "PgUp", key: "pageup", title: "Page up" },
  { label: "PgDn", key: "pagedown", title: "Page down" },
  { label: "Enter", key: "enter", title: "Enter" },
  { label: "Ctrl-C", key: "ctrl-c", title: "Control C", fixed: true },
];

/** The key name with the armed modifiers in front: `chord("up", {ctrl}) === "ctrl-up"`. */
export function chord(key: string, mods: ReadonlySet<Modifier>): string {
  const parts = MODIFIERS.filter((m) => mods.has(m));
  return [...parts, key].join("-");
}

/** Punctuation that the daemon names as a key (US layout). */
const PUNCT = new Set(["-", "=", "[", "]", "\\", ";", "'", ",", ".", "/", "`"]);

/**
 * A typed text while modifiers are armed: the first character as a chord
 * with the modifiers, the rest as a `text:` key. An upper-case letter adds
 * Shift (`A` is `shift-a`). Null when no modifier is armed, or when the
 * first character has no key name (an emoji, an escape sequence): then the
 * text goes as typed.
 */
export function typedKeys(data: string, mods: ReadonlySet<Modifier>): string[] | null {
  if (!mods.size || !data) return null;
  const [first, ...rest] = [...data];
  const c = first!;
  let key: string;
  const m = new Set(mods);
  if (/^[a-z0-9]$/.test(c)) key = c;
  else if (/^[A-Z]$/.test(c)) {
    key = c.toLowerCase();
    m.add("shift");
  } else if (c === " ") key = "space";
  else if (PUNCT.has(c)) key = c;
  else if (c === "\r") key = "enter";
  else if (c === "\t") key = "tab";
  else return null;
  const keys = [chord(key, m)];
  const tail = rest.join("");
  if (tail) keys.push(`text:${tail}`);
  return keys;
}

/** The daemon's limit: keys to one agent in one second (`TERMINAL_KEYS_PER_SEC`). */
export const KEYS_PER_SEC = 20;

/**
 * The client side of the rate limit: at most `limit` keys in any window of
 * one second. A sliding window that keeps to the limit also keeps to the
 * daemon's fixed window. `take` returns false, and takes nothing, when the
 * keys would go over.
 */
export class KeyRate {
  private times: number[] = [];
  constructor(
    private readonly limit = KEYS_PER_SEC,
    private readonly now: () => number = () => Date.now(),
  ) {}

  take(n: number): boolean {
    const t = this.now();
    this.times = this.times.filter((x) => t - x < 1000);
    if (this.times.length + n > this.limit) return false;
    for (let i = 0; i < n; i++) this.times.push(t);
    return true;
  }
}
