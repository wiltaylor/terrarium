import { createSignal, For, Show } from "solid-js";
import emoji from "../lib/emoji.json";

// GitHub gemoji aliases, bundled locally. See lib/emoji-source.md.
const entries = Object.entries(emoji);

export function EmojiPicker(props: { onPick: (shortcode: string) => void }) {
  const [open, setOpen] = createSignal(false);
  const [query, setQuery] = createSignal("");
  return <div>
    <button type="button" aria-expanded={open()} onClick={() => setOpen(!open())}>Emoji</button>
    <Show when={open()}>
      <div role="group" aria-label="Emoji picker" onKeyDown={(e) => { if (e.key === "Escape") setOpen(false); }}>
        <input aria-label="Search emoji" placeholder="Search GitHub emoji names" value={query()} onInput={(e) => setQuery(e.currentTarget.value)} />
        <div style={{ "max-height": "180px", overflow: "auto", display: "flex", "flex-wrap": "wrap" }}>
          <For each={entries.filter(([name]) => name.includes(query().trim().toLowerCase()))} fallback={<span>No emoji found.</span>}>
            {([name, glyph]) => <button type="button" title={`:${name}:`} aria-label={`:${name}:`} onClick={() => { props.onPick(`:${name}:`); setOpen(false); }}>{glyph}</button>}
          </For>
        </div>
      </div>
    </Show>
  </div>;
}
