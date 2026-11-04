// The on-screen keys under the agent terminal (issue #12): Esc, Tab, the
// arrows, Home/End, PgUp/PgDn, Enter, Ctrl-C, and the sticky modifiers
// Ctrl, Alt, and Shift. A phone keyboard has none of them. The keys go
// through `POST /api/agents/{id}/keys` (see ./terminalKeys for the names and
// the rate limit). A button does not take the focus, so the phone
// keyboard of the terminal stays open.

import { createSignal, For, onCleanup, Show, type Accessor } from "solid-js";
import { ApiError } from "../api/http";
import { chord, KEYPAD, KeyRate, MODIFIERS, type Modifier, type PadKey } from "./terminalKeys";

export interface KeyPadProps {
  /** Send key names to the agent. */
  send: (keys: string[]) => Promise<unknown>;
  /** False: the buttons are off (input is off, or the terminal is not connected). */
  enabled: boolean;
  /** The armed modifiers, owned by the terminal: a typed character uses them too. */
  mods: ReadonlySet<Modifier>;
  onToggle: (m: Modifier) => void;
  /** After a key with modifiers went out: clear them. */
  onUsed: () => void;
  /** The rate limit, shared with the typed chords of the terminal. */
  rate: KeyRate;
}

const MOD_LABEL: Record<Modifier, string> = { ctrl: "Ctrl", alt: "Alt", shift: "Shift" };

/** The state of a key pad: the armed modifiers and the rate limit. The owner gives it to the pad. */
export interface KeyPadState {
  mods: Accessor<ReadonlySet<Modifier>>;
  toggle: (m: Modifier) => void;
  clear: () => void;
  rate: KeyRate;
}

export function createKeyPadState(): KeyPadState {
  const [mods, setMods] = createSignal<ReadonlySet<Modifier>>(new Set());
  return {
    mods,
    toggle: (m) =>
      setMods((cur) => {
        const next = new Set(cur);
        if (next.has(m)) next.delete(m);
        else next.add(m);
        return next;
      }),
    clear: () => setMods(new Set<Modifier>()),
    rate: new KeyRate(),
  };
}

/** The text of a failed send. */
export function keyError(e: unknown): string {
  if (e instanceof ApiError && e.status === 429) return "Too many keys in one second. Wait a moment.";
  if (e instanceof ApiError) return e.message;
  return String(e);
}

export function KeyPad(props: KeyPadProps) {
  const [note, setNote] = createSignal<string | null>(null);
  let timer: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(timer));
  const flash = (text: string) => {
    setNote(text);
    clearTimeout(timer);
    timer = setTimeout(() => setNote(null), 4000);
  };

  const press = (k: PadKey) => {
    if (!props.enabled) return;
    const name = k.fixed ? k.key : chord(k.key, props.mods);
    if (!props.rate.take(1)) {
      flash("Too many keys in one second. Wait a moment.");
      return;
    }
    if (!k.fixed) props.onUsed();
    props.send([name]).catch((e: unknown) => flash(keyError(e)));
  };

  return (
    <div class="keypad" role="toolbar" aria-label="Terminal keys">
      <For each={MODIFIERS}>
        {(m) => (
          <button
            type="button"
            class="keypad-key keypad-mod"
            classList={{ armed: props.mods.has(m) }}
            aria-pressed={props.mods.has(m)}
            title={`${MOD_LABEL[m]}: applies to the next key or typed character`}
            disabled={!props.enabled}
            // Keep the focus in the terminal: the phone keyboard stays open.
            onPointerDown={(e) => e.preventDefault()}
            onClick={() => props.onToggle(m)}
          >
            {MOD_LABEL[m]}
          </button>
        )}
      </For>
      <For each={KEYPAD}>
        {(k) => (
          <button
            type="button"
            class="keypad-key"
            classList={{ "keypad-danger": k.key === "ctrl-c" }}
            title={k.title}
            aria-label={k.title}
            disabled={!props.enabled}
            onPointerDown={(e) => e.preventDefault()}
            onClick={() => press(k)}
          >
            {k.label}
          </button>
        )}
      </For>
      <Show when={note()}>
        <span class="small keypad-note" role="status">
          {note()}
        </span>
      </Show>
    </div>
  );
}
