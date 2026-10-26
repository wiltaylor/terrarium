// The spec part of the trigger dialog (docs/api.md §19.4): the kinds with
// suggestions, the match rules, the optional dedupe and escalation, and
// max_per_hour. The state and the mapping to the spec are in ./form and
// ./trigger; this file only draws them.

import { For, Index, Show } from "solid-js";
import type { MatchOp } from "../../api/types";
import { agents, projects } from "../../stores";
import type { ScheduleDialogState } from "./form";
import type { FieldKit } from "./fields";
import { KIND_SUGGESTIONS, MATCH_OPS, PATH_SUGGESTIONS, splitList, TEMPLATE_PLACEHOLDERS } from "./trigger";

export function TriggerSpecFields(props: { dlg: ScheduleDialogState; kit: FieldKit }) {
  const dlg = props.dlg;
  const f = dlg.form;
  const { field, text, recipient } = props.kit;
  const has = (k: string) => splitList(f().kinds).includes(k);
  const addKind = (k: string) => {
    if (!has(k)) dlg.set("kinds", [...splitList(f().kinds), k].join(", "));
  };

  return (
    <fieldset class="stack" aria-label="Trigger">
      <legend class="small">On events</legend>
      {field(
        "kinds",
        "Event kinds",
        <input class="mono" aria-label="Event kinds" list="trigger-kinds" value={f().kinds} placeholder="ci.run.failed, ci.job.*" onInput={(e) => dlg.set("kinds", e.currentTarget.value)} />,
        () => "Globs, split by commas: * is any text, ? is one character.",
      )}
      <datalist id="trigger-kinds">
        <For each={KIND_SUGGESTIONS}>{(k) => <option value={k} />}</For>
      </datalist>
      <div class="row small" style={{ "flex-wrap": "wrap", gap: "4px" }} aria-label="Kind suggestions">
        <span class="muted">Add:</span>
        <For each={KIND_SUGGESTIONS}>
          {(k) => (
            <button type="button" class="chip mono" disabled={has(k)} onClick={() => addKind(k)}>
              {k}
            </button>
          )}
        </For>
      </div>

      <div class="stack" style={{ gap: "4px" }} classList={{ "field-bad": !!dlg.fieldError("rules") }}>
        <span class="small">
          <strong>Match rules</strong> <span class="muted">· every rule must hold; none matches every event of the kinds</span>
        </span>
        <Index each={f().rules}>
          {(r, i) => (
            <div class="row" style={{ gap: "6px", "flex-wrap": "wrap" }} aria-label={`Rule ${i + 1}`}>
              <input class="mono" aria-label={`Rule ${i + 1} path`} list="trigger-paths" value={r().path} placeholder="data.branch" style={{ flex: "1 1 10rem" }} onInput={(e) => dlg.setRule(i, { path: e.currentTarget.value })} />
              <select aria-label={`Rule ${i + 1} op`} value={r().op} onChange={(e) => dlg.setRule(i, { op: e.currentTarget.value as MatchOp })}>
                <For each={MATCH_OPS}>
                  {(o) => (
                    <option value={o.id} selected={o.id === r().op}>
                      {o.label}
                    </option>
                  )}
                </For>
              </select>
              <input
                class="mono"
                aria-label={`Rule ${i + 1} value`}
                value={r().value}
                placeholder={r().op === "in" ? "wil/terrarium, wil/site" : r().op === "glob" ? "release/*" : "main"}
                style={{ flex: "1 1 10rem" }}
                onInput={(e) => dlg.setRule(i, { value: e.currentTarget.value })}
              />
              <button type="button" class="ghost small" aria-label={`Remove rule ${i + 1}`} onClick={() => dlg.removeRule(i)}>
                ✕
              </button>
            </div>
          )}
        </Index>
        <datalist id="trigger-paths">
          <For each={PATH_SUGGESTIONS}>{(p) => <option value={p} />}</For>
        </datalist>
        <div>
          <button type="button" class="small" onClick={() => dlg.addRule()}>
            Add rule
          </button>
        </div>
        <Show when={dlg.fieldError("rules")}>
          {(m) => (
            <span class="field-error small" role="alert" aria-label="Error in Match rules">
              {m()}
            </span>
          )}
        </Show>
      </div>

      <label class="row small">
        <input type="checkbox" checked={f().dedupe} onChange={(e) => dlg.set("dedupe", e.currentTarget.checked)} />
        dedupe: one run per key in a window
      </label>
      <Show when={f().dedupe}>
        <div class="form-grid">
          {text("dedupeKey", "Dedupe key", "{{event.data.repo}}:{{event.data.branch}}")}
          {text("dedupeWindow", "Window", "10m", () => "90s, 10m, 1h; 1s to 7d")}
        </div>
      </Show>

      <label class="row small">
        <input type="checkbox" checked={f().escalate} onChange={(e) => dlg.set("escalate", e.currentTarget.checked)} />
        escalate: after a number of matches in a row, mail someone once instead
      </label>
      <Show when={f().escalate}>
        <div class="form-grid">
          {text("escalateKey", "Escalation key", "{{event.data.repo}}")}
          {text("escalateAfter", "After (matches in a row)", "3")}
          {text("escalateResetOn", "Reset on (kinds)", "ci.run.succeeded", () => "An event of these kinds with the same key sets the count to zero.")}
          {recipient("escalateTo", "Escalate to", projects.projects(), agents.state.agents, "operator (the default)", true)}
        </div>
      </Show>

      <div class="form-grid">{text("maxPerHour", "Max runs per hour", "30", () => "1 to 600. More matches are skipped: rate limit.")}</div>
    </fieldset>
  );
}

/** The placeholders of the action strings, as a short list. */
export function TemplateHelp() {
  return (
    <details class="small" aria-label="Template placeholders">
      <summary class="muted">Placeholders for the subject, the body, a command argument, and the card key, title, and body</summary>
      <ul style={{ margin: "4px 0", "padding-left": "1.2rem" }}>
        <For each={TEMPLATE_PLACEHOLDERS}>
          {(p) => (
            <li>
              <code>{p.text}</code> <span class="muted">{p.meaning}</span>
            </li>
          )}
        </For>
      </ul>
      <span class="muted">A missing value is empty. The daemon quotes event text in a mail or a card, and each command argument stays one argument.</span>
    </details>
  );
}
