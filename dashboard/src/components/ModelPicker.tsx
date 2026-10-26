// A searchable model combobox: a text field that filters the list as you
// type (id and name, case-insensitive). The text is the value, so an id that
// is not in the list stays as typed. The logic is in ./modelPicker.ts.

import { createMemo, createSignal, createUniqueId, For, Show } from "solid-js";
import type { ModelInfo } from "../api/types";
import { afterInput, CLOSED, contextText, filterModels, moreText, notInList, pickerKey, type PickerState } from "./modelPicker";

export interface ModelPickerProps {
  /** The model id, or "" for the default. */
  value: string;
  onChange: (id: string) => void;
  /** The models to offer. Empty: a plain text field. */
  models: readonly ModelInfo[];
  /** The default the daemon picks for "", if known. Shown as the first row and the placeholder. */
  defaultModel?: string | null;
  /** The placeholder when there is no default. */
  placeholder?: string;
  label?: string;
}

/** One row of the list: the default (id "") or a model. */
interface Row {
  id: string;
  model?: ModelInfo;
}

export function ModelPicker(props: ModelPickerProps) {
  const base = createUniqueId();
  const listId = `${base}-list`;
  const optId = (i: number) => `${base}-opt-${i}`;
  const [state, setState] = createSignal<PickerState>(CLOSED);
  // The filter text: only what the user typed since the list opened. A
  // list opened with the arrow keys or a click shows every model.
  const [query, setQuery] = createSignal<string | null>(null);

  const filtered = createMemo(() => filterModels(props.models, query() ?? ""));
  const rows = createMemo<Row[]>(() => {
    const models = filtered().shown.filter((m) => m.id !== props.defaultModel);
    const head: Row[] = query() ? [] : [{ id: "" }];
    return [...head, ...models.map((m) => ({ id: m.id, model: m }))];
  });
  const hasList = () => props.models.length > 0;
  const open = () => state().open && hasList();
  const defaultText = () => (props.defaultModel ? `${props.defaultModel} (default)` : "the harness's default");

  const pick = (r: Row) => {
    props.onChange(r.id);
    setQuery(null);
    setState(CLOSED);
  };
  const openAll = () => {
    if (state().open) return;
    setQuery(null);
    const i = rows().findIndex((r) => r.id === props.value.trim());
    setState({ open: true, active: i });
  };

  const onKeyDown = (e: KeyboardEvent) => {
    if (!hasList()) return;
    // An arrow key opens a closed list on the current value, else on the
    // first (Down) or the last (Up) row.
    if (!state().open && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
      e.preventDefault();
      openAll();
      if (state().active < 0) setState(pickerKey(CLOSED, e.key, rows().length).state);
      return;
    }
    const r = pickerKey(state(), e.key, rows().length);
    if (!r.handled && r.state === state()) return;
    if (r.handled) {
      e.preventDefault();
      // Escape closes the list, not the modal around it.
      e.stopPropagation();
    }
    if (r.pick !== undefined) {
      const row = rows()[r.pick];
      if (row) return pick(row);
    }
    if (!r.state.open) setQuery(null);
    setState(r.state);
    if (r.state.active >= 0) document.getElementById(optId(r.state.active))?.scrollIntoView({ block: "nearest" });
  };

  return (
    <div class="model-picker">
      <input
        role={hasList() ? "combobox" : undefined}
        aria-label={props.label ?? "Model"}
        aria-autocomplete={hasList() ? "list" : undefined}
        aria-expanded={hasList() ? open() : undefined}
        aria-controls={hasList() ? listId : undefined}
        aria-activedescendant={open() && state().active >= 0 ? optId(state().active) : undefined}
        autocomplete="off"
        spellcheck={false}
        value={props.value}
        placeholder={props.defaultModel ?? props.placeholder}
        onInput={(e) => {
          props.onChange(e.currentTarget.value);
          setQuery(e.currentTarget.value);
          setState(afterInput());
        }}
        onClick={openAll}
        // Native, not delegated: stopPropagation then keeps Escape from the
        // window listener of the modal.
        on:keydown={onKeyDown}
        onBlur={() => {
          setQuery(null);
          setState(CLOSED);
        }}
      />
      <Show when={open()}>
        <div class="model-picker-pop">
          <ul class="model-picker-list" id={listId} role="listbox" aria-label={props.label ?? "Model"}>
            <For each={rows()}>
              {(r, i) => (
                <li
                  id={optId(i())}
                  role="option"
                  aria-selected={i() === state().active}
                  classList={{ active: i() === state().active, current: r.id === props.value.trim() }}
                  // Keep the focus in the input; a click picks the row.
                  onMouseDown={(e) => e.preventDefault()}
                  onClick={() => pick(r)}
                  onMouseMove={() => i() !== state().active && setState({ open: true, active: i() })}
                >
                  <Show when={r.model} fallback={<span>{defaultText()}</span>}>
                    {(m) => (
                      <>
                        <span class="model-picker-name">{m().name && m().name !== m().id ? m().name : m().id}</span>
                        <Show when={m().name && m().name !== m().id}>
                          <span class="mono small muted">{m().id}</span>
                        </Show>
                        <Show when={contextText(m())}>{(c) => <span class="small muted model-picker-ctx">{c()}</span>}</Show>
                      </>
                    )}
                  </Show>
                </li>
              )}
            </For>
          </ul>
          <Show when={!rows().length}>
            <div class="model-picker-note muted small">No model matches. The typed id stays as the value.</div>
          </Show>
          <Show when={moreText(filtered().more)}>{(t) => <div class="model-picker-note muted small">{t()}</div>}</Show>
        </div>
      </Show>
      <Show when={hasList() && notInList(props.value, props.models, props.defaultModel)}>
        <span class="muted small">Not in the list. The daemon passes the id as typed.</span>
      </Show>
    </div>
  );
}
