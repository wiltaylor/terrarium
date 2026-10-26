// The labelled fields of the schedule and trigger dialogs: each field shows
// the message of a refusal under it (the form's own check, or the daemon's
// `422` for the field that `details.field` names).

import { For, Show, type JSX } from "solid-js";
import type { Agent, Project } from "../../api/types";
import { recipientOptions, type FormKey, type ScheduleDialogState, type ScheduleForm } from "./form";

export function fieldKit(dlg: ScheduleDialogState) {
  const f = dlg.form;
  /** A labelled field with the message of a refusal under it. */
  // The hint and the error stay out of the <label>, so the input's name is the label alone.
  const field = (key: FormKey, label: string, input: JSX.Element, hint?: () => JSX.Element) => (
    <div class="field" classList={{ "field-bad": !!dlg.fieldError(key) }}>
      <label>
        {label}
        {input}
      </label>
      <Show when={hint?.()}>{(h) => <span class="small muted">{h()}</span>}</Show>
      <Show when={dlg.fieldError(key)}>
        {(m) => (
          <span class="field-error small" role="alert" aria-label={`Error in ${label}`}>
            {m()}
          </span>
        )}
      </Show>
    </div>
  );
  type TextKey = { [K in FormKey]: ScheduleForm[K] extends string ? K : never }[FormKey];
  const text = (key: TextKey, label: string, placeholder = "", hint?: () => JSX.Element) =>
    field(key, label, <input value={f()[key]} placeholder={placeholder} onInput={(e) => dlg.set(key, e.currentTarget.value)} />, hint);
  /** A mail address picker: the operator, the project inboxes, and the agents. `empty` is the text of the empty choice. */
  const recipient = (key: "mailTo" | "escalateTo", label: string, projects: Pick<Project, "name">[], agents: Pick<Agent, "project" | "name">[], empty: string, allowEmpty = false) =>
    field(
      key,
      label,
      <select aria-label={label} value={f()[key]} onChange={(e) => dlg.set(key, e.currentTarget.value)}>
        <option value="" disabled={!allowEmpty} selected={!f()[key]}>
          {empty}
        </option>
        <For each={recipientOptions(projects, agents, f().scope)}>
          {(g) => (
            <optgroup label={g.label}>
              <For each={g.options}>
                {(o) => (
                  <option value={o.value} selected={o.value === f()[key]}>
                    {o.label}
                  </option>
                )}
              </For>
            </optgroup>
          )}
        </For>
      </select>,
    );
  return { field, text, recipient };
}

export type FieldKit = ReturnType<typeof fieldKit>;
