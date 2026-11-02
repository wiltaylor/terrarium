// Add or change one block of the dashboard: a monitor block (the monitor,
// the kind that fits it, the size, the title, the options) or a Markdown
// note that labels a section.

import { createSignal, For, Show } from "solid-js";
import type { DashboardBlock, DashboardBlockKind, DashboardBlockSize, DashboardRange, Monitor } from "../../api/types";
import { Modal } from "../../components/ui";
import { BLOCK_KINDS, blockFor, kindsFor, noteBlock, RANGES, SIZES } from "./model";

export function BlockDialog(props: { block?: DashboardBlock; blocks: DashboardBlock[]; monitors: Monitor[]; onClose: () => void; onSave: (b: DashboardBlock) => void }) {
  const first = props.monitors[0];
  const [b, setB] = createSignal<DashboardBlock>(props.block ? structuredClone(props.block) : first ? blockFor(first, props.blocks) : noteBlock(props.blocks));
  const monitor = () => props.monitors.find((m) => m.id === b().monitor);
  const kinds = (): DashboardBlockKind[] => (b().kind === "note" ? ["note"] : monitor() ? kindsFor(monitor()!.kind) : []);
  const isNote = () => b().kind === "note";
  const setWhat = (v: string) => {
    if (v === "note") setB((x) => ({ ...noteBlock(props.blocks), id: x.id, title: x.title }));
    else {
      const m = props.monitors.find((x) => x.id === v);
      if (!m) return;
      setB((x) => {
        const fits = kindsFor(m.kind);
        const kind = x.kind !== "note" && fits.includes(x.kind) ? x.kind : fits[0]!;
        return { ...x, monitor: m.id, kind, text: undefined, size: x.kind === "note" ? blockFor(m, props.blocks).size : x.size };
      });
    }
  };
  const opt = <K extends keyof DashboardBlock["options"]>(k: K, v: DashboardBlock["options"][K]) => setB((x) => ({ ...x, options: { ...x.options, [k]: v } }));
  const save = (e: Event) => {
    e.preventDefault();
    const x = b();
    const clean: DashboardBlock = { id: x.id, kind: x.kind, size: x.size, options: {} };
    if (x.kind === "note") clean.text = x.text ?? "";
    else clean.monitor = x.monitor;
    if (x.title?.trim()) clean.title = x.title.trim();
    if (x.kind === "chart" && x.options.range) clean.options.range = x.options.range;
    if (x.kind === "stat" && x.options.sparkline === false) clean.options.sparkline = false;
    if (x.kind !== "note" && x.options.decimals !== undefined) clean.options.decimals = x.options.decimals;
    props.onSave(clean);
  };
  return (
    <Modal title={props.block ? "Edit block" : "Add block"} onClose={() => props.onClose()} class="modal-full">
      <form class="stack" onSubmit={save}>
        <label>
          Shows
          <select value={isNote() ? "note" : (b().monitor ?? "")} onChange={(e) => setWhat(e.currentTarget.value)}>
            <For each={props.monitors}>
              {(m) => (
                <option value={m.id}>
                  Monitor {m.name} ({m.kind})
                </option>
              )}
            </For>
            <option value="note">A note (Markdown section header)</option>
          </select>
        </label>
        <Show
          when={!isNote()}
          fallback={
            <label>
              Note (Markdown)
              <textarea class="mono" rows={5} value={b().text ?? ""} onInput={(e) => setB((x) => ({ ...x, text: e.currentTarget.value }))} />
            </label>
          }
        >
          <label>
            Block kind
            <select value={b().kind} onChange={(e) => setB((x) => ({ ...x, kind: e.currentTarget.value as DashboardBlockKind }))}>
              <For each={kinds()}>{(k) => <option value={k}>{BLOCK_KINDS.find((x) => x.id === k)?.label ?? k}</option>}</For>
            </select>
          </label>
          <label>
            Title <span class="muted small">(default: the monitor name)</span>
            <input value={b().title ?? ""} maxLength={120} onInput={(e) => setB((x) => ({ ...x, title: e.currentTarget.value }))} placeholder={monitor()?.name} />
          </label>
        </Show>
        <label>
          Size
          <select value={b().size} onChange={(e) => setB((x) => ({ ...x, size: e.currentTarget.value as DashboardBlockSize }))}>
            <For each={SIZES}>{(s) => <option value={s.id}>{s.label}</option>}</For>
          </select>
        </label>
        <Show when={b().kind === "chart"}>
          <label>
            Range
            <select value={b().options.range ?? "24h"} onChange={(e) => opt("range", e.currentTarget.value as DashboardRange)}>
              <For each={RANGES}>{(r) => <option value={r.id}>Last {r.label}</option>}</For>
            </select>
          </label>
        </Show>
        <Show when={b().kind === "stat"}>
          <label class="row small">
            <input type="checkbox" checked={b().options.sparkline !== false} onChange={(e) => opt("sparkline", e.currentTarget.checked)} /> Show the 24-hour sparkline
          </label>
        </Show>
        <Show when={!isNote()}>
          <label>
            Decimals
            <select value={b().options.decimals === undefined ? "" : String(b().options.decimals)} onChange={(e) => opt("decimals", e.currentTarget.value === "" ? undefined : Number(e.currentTarget.value))}>
              <option value="">As the value needs</option>
              <For each={[0, 1, 2, 3]}>{(d) => <option value={String(d)}>{d}</option>}</For>
            </select>
          </label>
        </Show>
        <div class="row" style={{ "justify-content": "flex-end" }}>
          <button type="button" class="ghost" onClick={() => props.onClose()}>
            Cancel
          </button>
          <button type="submit" class="primary" disabled={!isNote() && !monitor()}>
            {props.block ? "Apply" : "Add"}
          </button>
        </div>
      </form>
    </Modal>
  );
}
