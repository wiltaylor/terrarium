// The input of a secret field: a password input with a show toggle, or a
// textarea for a key. No autocomplete and no spell check, so the browser
// keeps no copy of the value.

import { createSignal, Show } from "solid-js";

export function SecretInput(props: { label: string; value: string; onInput: (v: string) => void; multiline?: boolean; placeholder?: string; required?: boolean }) {
  const [shown, setShown] = createSignal(false);
  return (
    <Show
      when={!props.multiline}
      fallback={
        <textarea
          class="mono small"
          rows={6}
          aria-label={props.label}
          value={props.value}
          placeholder={props.placeholder}
          required={props.required}
          autocomplete="off"
          spellcheck={false}
          onInput={(e) => props.onInput(e.currentTarget.value)}
        />
      }
    >
      <span class="row" style={{ gap: "4px" }}>
        <input
          type={shown() ? "text" : "password"}
          aria-label={props.label}
          value={props.value}
          placeholder={props.placeholder}
          required={props.required}
          autocomplete="new-password"
          spellcheck={false}
          style={{ flex: "1" }}
          onInput={(e) => props.onInput(e.currentTarget.value)}
        />
        <button type="button" class="small ghost" aria-pressed={shown()} aria-label={shown() ? `Hide ${props.label}` : `Show ${props.label}`} onClick={() => setShown(!shown())}>
          {shown() ? "Hide" : "Show"}
        </button>
      </span>
    </Show>
  );
}
