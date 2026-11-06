// The dialogs and the row menu of the project folders in the sidebar
// (docs/api.md §6.12, TER-48): a name for a new or renamed folder, Move to
// folder, and the delete of a folder.

import { createSignal, For, onCleanup, onMount, Show } from "solid-js";
import type { ProjectFolder, ProjectPlacement } from "../../api/types";
import { Modal } from "../../components/ui";
import { canMoveTo, folderList, type Moving } from "./model";

/** The name of a new folder, or the new name of a folder. */
export function FolderNameDialog(props: { title: string; initial?: string; save: string; onSave: (name: string) => Promise<unknown>; onClose: () => void }) {
  const [name, setName] = createSignal(props.initial ?? "");
  const [busy, setBusy] = createSignal(false);
  let input: HTMLInputElement | undefined;
  onMount(() => input?.focus());
  return (
    <Modal title={props.title} onClose={props.onClose}>
      <form
        class="stack"
        style={{ gap: "8px" }}
        onSubmit={async (e) => {
          e.preventDefault();
          if (!name().trim()) return;
          setBusy(true);
          try {
            await props.onSave(name().trim());
          } finally {
            setBusy(false);
          }
          props.onClose();
        }}
      >
        <label>
          Name
          <input ref={input} value={name()} maxLength={64} required onInput={(e) => setName(e.currentTarget.value)} />
        </label>
        <div class="row">
          <button type="submit" class="primary" disabled={busy() || !name().trim()}>
            {props.save}
          </button>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}

/**
 * Move to folder: the top level and each folder in tree order. The place
 * where the row is now, a folder itself and its subfolders, and a folder
 * that would nest too deep cannot be chosen.
 */
export function MoveToFolderDialog(props: {
  what: Moving;
  label: string;
  folders: ProjectFolder[];
  placements: ProjectPlacement[];
  onMove: (target: string | null) => Promise<unknown>;
  onClose: () => void;
}) {
  const ok = (target: string | null) => canMoveTo(props.folders, props.placements, props.what, target);
  const options = () => [{ id: null as string | null, name: "Projects (top level)", level: 0 }, ...folderList(props.folders).map((f) => ({ id: f.folder.id, name: f.folder.name, level: f.level }))];
  const [choice, setChoice] = createSignal<string | null | undefined>(options().find((o) => ok(o.id))?.id);
  const [busy, setBusy] = createSignal(false);
  return (
    <Modal title={`Move ${props.label} to folder`} onClose={props.onClose}>
      <form
        class="stack"
        style={{ gap: "8px" }}
        aria-label="Move to folder"
        onSubmit={async (e) => {
          e.preventDefault();
          const target = choice();
          if (target === undefined || !ok(target)) return;
          setBusy(true);
          try {
            await props.onMove(target);
          } finally {
            setBusy(false);
          }
          props.onClose();
        }}
      >
        <div class="folder-choices" role="radiogroup" aria-label="Folder">
          <For each={options()}>
            {(o) => (
              <label class="folder-choice" style={{ "padding-left": `${o.level * 14}px` }}>
                <input type="radio" name="folder" checked={choice() === o.id} disabled={!ok(o.id)} onChange={() => setChoice(o.id)} />
                <span>{o.name}</span>
              </label>
            )}
          </For>
        </div>
        <Show when={props.folders.length === 0}>
          <p class="muted small">No folders yet. Add one with the + of Projects.</p>
        </Show>
        <div class="row">
          <button type="submit" class="primary" disabled={busy() || choice() === undefined || !ok(choice() ?? null)}>
            Move
          </button>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}

/** The delete of a folder: its subfolders and projects move up one level. */
export function DeleteFolderDialog(props: { name: string; projects: number; onDelete: () => Promise<unknown>; onClose: () => void }) {
  const [busy, setBusy] = createSignal(false);
  return (
    <Modal title={`Delete the folder ${props.name}`} onClose={props.onClose}>
      <p>
        {props.projects > 0
          ? `Its ${props.projects} project(s) and its subfolders move up one level. No project is deleted.`
          : "Its subfolders, if any, move up one level."}
      </p>
      <div class="row">
        <button
          class="danger"
          disabled={busy()}
          onClick={async () => {
            setBusy(true);
            try {
              await props.onDelete();
            } finally {
              setBusy(false);
            }
            props.onClose();
          }}
        >
          Delete folder
        </button>
        <button onClick={() => props.onClose()}>Cancel</button>
      </div>
    </Modal>
  );
}

export interface MenuItem {
  label: string;
  /** The key on the row that does the same (aria-keyshortcuts), for the title. */
  key?: string;
  run: () => void;
}

/**
 * The "⋯" of a project or folder row: a small menu. As the cog, it is not a
 * tree row, so it is not in the roving tabindex (tabindex -1). The keyboard
 * runs each item with its key on the row.
 */
export function RowMenu(props: { label: string; items: MenuItem[] }) {
  const [open, setOpen] = createSignal(false);
  let box: HTMLSpanElement | undefined;
  const away = (e: MouseEvent) => {
    if (box && !box.contains(e.target as Node)) setOpen(false);
  };
  onMount(() => document.addEventListener("mousedown", away));
  onCleanup(() => document.removeEventListener("mousedown", away));
  return (
    <span class="tree-menu" ref={box}>
      <button
        type="button"
        class="tree-menu-button"
        tabIndex={-1}
        aria-haspopup="menu"
        aria-expanded={open()}
        aria-label={props.label}
        title={props.label}
        onClick={(e) => {
          e.preventDefault();
          e.stopPropagation();
          setOpen(!open());
        }}
      >
        ⋯
      </button>
      <Show when={open()}>
        <ul class="tree-menu-list" role="menu" aria-label={props.label}>
          <For each={props.items}>
            {(item) => (
              <li role="none">
                <button
                  type="button"
                  role="menuitem"
                  tabIndex={-1}
                  title={item.key ? `${item.label} (${item.key})` : item.label}
                  onClick={(e) => {
                    e.preventDefault();
                    e.stopPropagation();
                    setOpen(false);
                    item.run();
                  }}
                >
                  {item.label}
                </button>
              </li>
            )}
          </For>
        </ul>
      </Show>
    </span>
  );
}
