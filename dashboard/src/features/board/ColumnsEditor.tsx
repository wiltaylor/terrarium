import { createSignal, For, Index, onMount, Show } from "solid-js";
import { ErrorBanner, Modal } from "../../components/ui";
import type { Board, Column, ColumnRole } from "../../api/types";
import {
  addRow,
  canMove,
  checkColumns,
  type ColumnRow,
  defaultTitle,
  dragRow,
  isClean,
  isFixed,
  moveRow,
  overAfterSave,
  removeBlocker,
  rowsFromBoard,
  setRole,
  TITLE_MAX,
  toEdits,
} from "./columns";

/**
 * The Edit columns dialog of the board (docs/api.md §8.1). Save sends the whole list, with `rename_from`
 * for a renamed column. `backlog` and `done` stay first and last, and take no WIP limit and no role.
 */
/** `autoFocus` (default true): the dialog takes focus. The page widget (TER-1077) does not, so a rights refresh never moves focus. */
type ColumnsEditorProps = { columns: Column[]; onSave: (edits: ReturnType<typeof toEdits>) => Promise<Board>; onClose: () => void; autoFocus?: boolean };
export function ColumnsEditor(props: ColumnsEditorProps) {
  return <Modal title="Edit columns" class="modal-full columns-editor-modal" onClose={() => props.onClose()}>
    <ColumnsEditorFields {...props} />
  </Modal>;
}
export function ColumnsEditorFields(props: ColumnsEditorProps) {
  const [rows, setRows] = createSignal<ColumnRow[]>(rowsFromBoard(props.columns));
  // The check shows after the first save, and then follows each change.
  const [checked, setChecked] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const [dragFrom, setDragFrom] = createSignal<number | null>(null);
  const check = () => checkColumns(rows());
  const errorsOf = (key: string) => (checked() ? (check().rows[key] ?? {}) : {});
  let listEl: HTMLOListElement | undefined;
  onMount(() => { if (props.autoFocus !== false) listEl?.querySelector<HTMLInputElement>("input:not([disabled])")?.focus(); });

  const patch = (i: number, p: Partial<ColumnRow>) => setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...p } : r)));

  const save = async () => {
    setChecked(true);
    if (!isClean(check()) || busy()) return;
    setBusy(true);
    try {
      await props.onSave(toEdits(rows()));
      props.onClose();
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
      <form
        class="stack columns-editor"
        onSubmit={(e) => {
          e.preventDefault();
          void save();
        }}
      >
        <p class="muted small" style={{ margin: 0 }}>
          Backlog is always first and Done always last. One column has the role Working: <code>card claim</code> moves cards there. A move to the
          Blocked column needs a comment. A WIP limit stops more cards from entering the column.
        </p>
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <Show when={checked() && check().list.length}>
          <ul class="field-error small" role="alert" style={{ margin: 0 }}>
            <For each={check().list}>{(m) => <li>{m}</li>}</For>
          </ul>
        </Show>
        <ol class="columns-editor-list" ref={listEl}>
          <Index each={rows()}>
            {(row, i) => {
              const fixed = () => isFixed(row().original ?? "");
              const blocker = () => removeBlocker(row());
              const errs = () => errorsOf(row().key);
              return (
                <li
                  class="columns-editor-row"
                  classList={{ fixed: fixed(), dragging: dragFrom() === i }}
                  data-column-row={row().name || "new"}
                  onDragOver={(e) => {
                    if (dragFrom() === null) return;
                    e.preventDefault();
                  }}
                  onDrop={(e) => {
                    e.preventDefault();
                    const from = dragFrom();
                    setDragFrom(null);
                    if (from !== null) setRows((rs) => dragRow(rs, from, i));
                  }}
                >
                  <div class="columns-editor-move">
                    <span
                      class="drag-handle"
                      classList={{ disabled: fixed() }}
                      draggable={!fixed()}
                      title={fixed() ? "A fixed column does not move" : "Drag to move"}
                      aria-hidden="true"
                      onDragStart={(e) => {
                        e.dataTransfer?.setData("text/plain", String(i));
                        setDragFrom(i);
                      }}
                      onDragEnd={() => setDragFrom(null)}
                    >
                      ⋮⋮
                    </span>
                    <button type="button" class="ghost small" disabled={!canMove(rows(), i, -1)} aria-label={`Move ${row().name || "the new column"} up`} onClick={() => setRows((rs) => moveRow(rs, i, -1))}>
                      ↑
                    </button>
                    <button type="button" class="ghost small" disabled={!canMove(rows(), i, 1)} aria-label={`Move ${row().name || "the new column"} down`} onClick={() => setRows((rs) => moveRow(rs, i, 1))}>
                      ↓
                    </button>
                  </div>
                  <label classList={{ "field-bad": !!errs().name }}>
                    <span class="small muted">Name</span>
                    <input
                      class="mono"
                      value={row().name}
                      disabled={fixed()}
                      placeholder="qa"
                      aria-invalid={!!errs().name}
                      onInput={(e) => patch(i, { name: e.currentTarget.value })}
                    />
                    <Show when={errs().name}>
                      <span class="field-error small">{errs().name}</span>
                    </Show>
                    <Show when={row().original && row().original !== row().name.trim() && !errs().name}>
                      <span class="small muted">Renamed from {row().original}; the cards stay.</span>
                    </Show>
                  </label>
                  <label classList={{ "field-bad": !!errs().title }}>
                    <span class="small muted">Title</span>
                    <input value={row().title} maxLength={TITLE_MAX + 10} placeholder={defaultTitle(row().name.trim() || "title")} aria-invalid={!!errs().title} onInput={(e) => patch(i, { title: e.currentTarget.value })} />
                    <Show when={errs().title}>
                      <span class="field-error small">{errs().title}</span>
                    </Show>
                  </label>
                  <label classList={{ "field-bad": !!errs().wip }}>
                    <span class="small muted">WIP limit</span>
                    <input
                      type="number"
                      min="1"
                      step="1"
                      inputMode="numeric"
                      value={row().wip}
                      disabled={fixed()}
                      title={fixed() ? `${row().original} takes no WIP limit` : undefined}
                      placeholder={fixed() ? "—" : "none"}
                      aria-invalid={!!errs().wip}
                      onInput={(e) => patch(i, { wip: e.currentTarget.value })}
                    />
                    <Show when={errs().wip}>
                      <span class="field-error small">{errs().wip}</span>
                    </Show>
                    <Show when={!errs().wip && overAfterSave(row())}>
                      <span class="small warn-text">Over the limit: {row().count} cards. No more cards enter until some leave.</span>
                    </Show>
                  </label>
                  <label>
                    <span class="small muted">Role</span>
                    <select
                      value={row().role}
                      disabled={fixed()}
                      title={fixed() ? `${row().original} takes no role` : undefined}
                      onChange={(e) => setRows((rs) => setRole(rs, i, e.currentTarget.value as ColumnRole | ""))}
                    >
                      <option value="">none</option>
                      <option value="working">Working</option>
                      <option value="blocked">Blocked</option>
                      <option value="validating">Validating</option>
                      <option value="review">Review</option>
                      <option value="merge_queue">Merge queue</option>
                      <option value="accepted">Accepted</option>
                    </select>
                  </label>
                  <div class="columns-editor-end">
                    <span class="small muted">{row().count === 1 ? "1 card" : `${row().count} cards`}</span>
                    <button
                      type="button"
                      class="ghost small"
                      disabled={!!blocker()}
                      title={blocker() ?? `Remove ${row().name || "this column"}`}
                      aria-label={`Remove ${row().name || "the new column"}`}
                      onClick={() => setRows((rs) => rs.filter((_, j) => j !== i))}
                    >
                      Remove
                    </button>
                  </div>
                </li>
              );
            }}
          </Index>
        </ol>
        <div>
          <button type="button" onClick={() => setRows((rs) => addRow(rs))}>
            Add column
          </button>
        </div>
        <div class="row">
          <button class="primary" type="submit" disabled={busy()}>
            Save columns
          </button>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
        </div>
      </form>
  );
}
