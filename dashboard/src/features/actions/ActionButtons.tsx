// The buttons of the project actions (TER-53, docs/api.md §19.10), on the
// project dashboard and next to the project name in Home 'Work in
// progress'. A click opens the confirm step: what the action does, who it
// runs as, and for an exec the target, the time limit, and the run history
// link. "Run" posts the run and shows its result in the same dialog.
//
// The dashboard acts as the operator, who may run every action. A disabled
// action shows as a disabled button with its reason on the dashboard; Home
// lists only the enabled ones.

import { A } from "@solidjs/router";
import { createSignal, For, Show } from "solid-js";
import type { Schedule } from "../../api/types";
import { Modal } from "../../components/ui";
import { api } from "../../stores";
import { confirmText, failureOf, historyHref, limitWords, offReason, resultOf, type RunResult } from "./model";

export function ActionButtons(props: { actions: Schedule[]; compact?: boolean; onRan?: () => void }) {
  const [open, setOpen] = createSignal<Schedule | null>(null);
  return (
    <Show when={props.actions.length > 0}>
      <span class={`action-buttons row${props.compact ? " compact" : ""}`} role="group" aria-label="Project actions">
        <For each={props.actions}>
          {(s) => {
            const off = () => offReason(s);
            return (
              <button class={props.compact ? "small" : undefined} data-action={s.name} disabled={!!off()} title={off() ?? `Run ${s.name}`} onClick={() => setOpen(s)}>
                ▶ {s.name}
              </button>
            );
          }}
        </For>
      </span>
      <Show when={open()}>{(s) => <RunDialog action={s()} onClose={() => setOpen(null)} onRan={props.onRan} />}</Show>
    </Show>
  );
}

function RunDialog(props: { action: Schedule; onClose: () => void; onRan?: () => void }) {
  const t = confirmText(props.action);
  const [busy, setBusy] = createSignal(false);
  const [result, setResult] = createSignal<RunResult | null>(null);
  const run = async () => {
    setBusy(true);
    try {
      setResult(resultOf(await api.schedules.run(props.action.id)));
    } catch (e) {
      setResult(failureOf(e));
    } finally {
      setBusy(false);
      props.onRan?.();
    }
  };
  return (
    <Modal title={`Run ${props.action.name}?`} onClose={props.onClose} class="action-run">
      <div class="stack">
        <p>
          <strong>{t.what}</strong>
          <br />
          <span class="small muted">Runs as {t.as}.</span>
        </p>
        <Show when={t.exec}>
          {(x) => (
            <p class="small notice" data-exec-note>
              This runs <code>{x().cmd}</code> on the target <strong>{x().target}</strong>, for up to {limitWords(x().limitSecs)}. This dialog waits for the end. You can close it: the run goes on, and the{" "}
              <A href={historyHref(props.action)}>run history</A> has the result.
            </p>
          )}
        </Show>
        <Show when={result()}>
          {(r) => (
            <div class="action-result" role="status" data-status={r().status}>
              <span class={`chip ${r().tone}`}>{r().status}</span> {r().summary}
              <Show when={r().error}>
                <div class="small bad">{r().error}</div>
              </Show>
              <div class="small">
                <A href={historyHref(props.action)}>Run history</A>
              </div>
            </div>
          )}
        </Show>
        <div class="row" style={{ "justify-content": "flex-end" }}>
          <button class="ghost" onClick={() => props.onClose()}>
            {result() ? "Close" : "Cancel"}
          </button>
          <Show when={!result()}>
            <button class="primary" disabled={busy()} onClick={() => void run()}>
              {busy() ? "Running…" : "Run"}
            </button>
          </Show>
        </div>
      </div>
    </Modal>
  );
}
