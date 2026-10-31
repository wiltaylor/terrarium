// The diff viewer of the repo browser (docs/api.md §5.3): a file list
// with +/- counts, then one collapsible block for each file, with a sticky
// header and the old and new line numbers. Every file name and every line
// is a text node: an agent wrote them.

import { createMemo, For, Show } from "solid-js";
import type { DiffSet, FileDiff } from "../api/types";
import { parsePatch } from "../lib/diff";
import { Empty } from "./ui";

const STATUS: Record<FileDiff["status"], string> = { added: "A", deleted: "D", modified: "M", typechange: "T", mode: "mode", unmerged: "U", refused: "!" };

function FileBlock(props: { file: FileDiff; open: boolean }) {
  const rows = createMemo(() => parsePatch(props.file.patch));
  return (
    <details class="diff-file" open={props.open}>
      <summary class="diff-head">
        <span class={`diff-status st-${props.file.status}`} title={props.file.status}>
          {STATUS[props.file.status]}
        </span>
        <span class="mono diff-path">{props.file.path}</span>
        <span class="diff-counts mono">
          <span class="diff-add-n">+{props.file.additions}</span> <span class="diff-del-n">−{props.file.deletions}</span>
        </span>
      </summary>
      <Show when={props.file.binary}>
        <p class="small muted diff-note">Binary file: no text diff.</p>
      </Show>
      <Show when={!props.file.binary && !props.file.patch && !props.file.note}>
        <p class="small muted diff-note">{props.file.status === "mode" ? "The file mode changed." : "No text change."}</p>
      </Show>
      <Show when={rows().length}>
        <div class="code-scroll">
          <table class="diff-table">
            <tbody>
              <For each={rows()}>
                {(r) => (
                  <tr class={`diff-${r.kind}`}>
                    <td class="ln">{r.old ?? ""}</td>
                    <td class="ln">{r.new ?? ""}</td>
                    <td class="mark">{r.kind === "add" ? "+" : r.kind === "del" ? "−" : ""}</td>
                    <td class="code">{r.text}</td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </Show>
      <Show when={props.file.note}>
        <p class="small muted diff-note">{props.file.note}</p>
      </Show>
      <Show when={props.file.truncated && !props.file.note}>
        <p class="small muted diff-note">The diff of this file is cut: at most 2000 lines for each file and 20 000 for each answer.</p>
      </Show>
    </details>
  );
}

/**
 * A set of file diffs. Files open by default when there are 30 or fewer.
 * With `onOpen`, each name in the file list that is still in the tree is a
 * link to the file.
 */
export function DiffView(props: { set: DiffSet; empty?: string; onOpen?: (path: string) => void }) {
  const totals = createMemo(() => props.set.files.reduce((t, f) => ({ a: t.a + f.additions, d: t.d + f.deletions }), { a: 0, d: 0 }));
  return (
    <Show when={props.set.files.length} fallback={<Empty>{props.empty ?? "No changes."}</Empty>}>
      <div class="diff-set stack" style={{ gap: "8px" }}>
        <p class="small muted" style={{ margin: 0 }}>
          {props.set.files.length} {props.set.files.length === 1 ? "file" : "files"} · <span class="diff-add-n">+{totals().a}</span> <span class="diff-del-n">−{totals().d}</span>
          {props.set.truncated ? " · cut: a cap was reached" : ""}
        </p>
        <ul class="diff-index small">
          <For each={props.set.files}>
            {(f) => (
              <li>
                <span class={`diff-status st-${f.status}`}>{STATUS[f.status]}</span>{" "}
                <Show when={props.onOpen && f.status !== "deleted" && f.status !== "refused"} fallback={<span class="mono">{f.path}</span>}>
                  <button class="link mono diff-open" onClick={() => props.onOpen!(f.path)} title="Show the file in the Files tab">
                    {f.path}
                  </button>
                </Show>{" "}
                <span class="mono">
                  <span class="diff-add-n">+{f.additions}</span> <span class="diff-del-n">−{f.deletions}</span>
                </span>
              </li>
            )}
          </For>
        </ul>
        <For each={props.set.files}>{(f) => <FileBlock file={f} open={props.set.files.length <= 30} />}</For>
      </div>
    </Show>
  );
}
