import { useInstance } from "../../instance";
// The record dialog of the DB tab: a form made from the schema of the type,
// for a new record or an edit. Each kind has its input (features/db/model.ts
// converts the values). The errors of the daemon (`details.fields`) show
// under each input. A stale edit (`409 conflict`) offers the current record.

import { createResource, createSignal, For, Match, Show, Switch, type JSX } from "solid-js";
import { ApiError } from "../../api/http";
import type { DbField, DbRecord, DbType } from "../../api/types";
import { Markdown } from "../../components/Markdown";
import { ConfirmButton, ErrorBanner, Modal } from "../../components/ui";
import { ago } from "../../lib/format";
import { renderMarkdown } from "../../lib/markdown";

import { formData, newForm, patchData, quoteValue, recordForm, toInput, type FormState, type FormValue } from "./model";

/** The field errors of a `422 invalid` answer (`details.fields`). */
function fieldErrors(e: unknown): Record<string, string> | null {
  const f = (e as { details?: { fields?: unknown } } | null)?.details?.fields;
  return f && typeof f === "object" ? (f as Record<string, string>) : null;
}

/** `readOnly` (no write_db, TER-1134): the record shows without Save and Delete record. */
export function RecordModal(props: { project: string; type: DbType; record?: DbRecord; readOnly?: boolean; onClose: () => void; onSaved: (r: DbRecord) => void; onDeleted: () => void }) {
  const { api } = useInstance();
  const [record, setRecord] = createSignal<DbRecord | undefined>(props.record);
  const [form, setForm] = createSignal<FormState>(props.record ? recordForm(props.type.fields, props.record.data) : newForm(props.type.fields));
  const [errors, setErrors] = createSignal<Record<string, string>>({});
  const [error, setError] = createSignal<unknown>(null);
  const [conflict, setConflict] = createSignal<DbRecord | null>(null);
  const [busy, setBusy] = createSignal(false);
  const set = (name: string, v: FormValue) => setForm((f) => ({ ...f, [name]: v }));

  const save = async () => {
    const r = record();
    const { data, errors: local } = r ? patchData(props.type.fields, r.data, form()) : formData(props.type.fields, form());
    setErrors(local);
    if (Object.keys(local).length) return;
    setBusy(true);
    try {
      const saved = r ? await api.db.updateRecord(props.project, r.id, data, r.revision) : await api.db.createRecord(props.project, props.type.name, data);
      setError(null);
      setConflict(null);
      props.onSaved(saved);
    } catch (e) {
      const fe = fieldErrors(e);
      if (fe) setErrors(fe);
      const cur = (e as { details?: { record?: DbRecord } }).details?.record;
      if (e instanceof ApiError && e.status === 409 && cur) setConflict(cur);
      else setError(e);
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    const r = record();
    if (!r) return;
    try {
      await api.db.removeRecord(props.project, r.id);
      props.onDeleted();
    } catch (e) {
      setError(e);
    }
  };

  /** Take the current record of a conflict: the form shows it, and the next save starts from it. */
  const takeCurrent = () => {
    const c = conflict();
    if (!c) return;
    setRecord(c);
    setForm(recordForm(props.type.fields, c.data));
    setConflict(null);
    setErrors({});
  };

  return (
    <Modal title={record() ? `${props.type.name} record` : `New ${props.type.name} record`} onClose={props.onClose} class="db-modal">
      <form
        class="stack db-form"
        noValidate
        onSubmit={(e) => {
          e.preventDefault();
          if (!props.readOnly) void save();
        }}
      >
        <Show when={record()}>
          {(r) => (
            <div class="muted small">
              <span class="mono">{r().id}</span> · rev {r().revision} · {r().updated_by_name}, {ago(r().updated_at)}
            </div>
          )}
        </Show>
        <Show when={conflict()}>
          {(c) => (
            <div class="error-banner" role="alert">
              <span style={{ flex: 1 }}>
                Someone changed this record since you opened it (now rev {c().revision}, by {c().updated_by_name}). Your changes are not saved.
              </span>
              <button type="button" class="small" onClick={takeCurrent}>
                Load the current record
              </button>
            </div>
          )}
        </Show>
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <Show when={errors()["*"]}>
          <div class="field-error small">{errors()["*"]}</div>
        </Show>
        <div class="db-fields">
          <For each={props.type.fields}>
            {(f) => (
              <div class={`field db-field db-field-${f.kind}${errors()[f.name] ? " field-bad" : ""}`}>
                <FieldInput project={props.project} field={f} value={form()[f.name] ?? toInput(f, undefined)} onChange={(v) => set(f.name, v)} />
                <Show when={errors()[f.name]}>
                  <span class="field-error small">
                    {f.name} {errors()[f.name]}
                  </span>
                </Show>
              </div>
            )}
          </For>
        </div>
        <div class="row db-actions">
          <Show when={!props.readOnly}>
            <button type="submit" class="primary" disabled={busy()}>
              {record() ? "Save" : "Create record"}
            </button>
          </Show>
          <button type="button" onClick={props.onClose}>
            {props.readOnly ? "Close" : "Cancel"}
          </button>
          <span style={{ flex: 1 }} />
          <Show when={record() && !props.readOnly}>
            <ConfirmButton label="Delete record" onConfirm={remove} />
          </Show>
        </div>
      </form>
    </Modal>
  );
}

/** The label of an input: the name, a required mark, and the description. */
function FieldLabel(props: { field: DbField; for?: string }) {
  return (
    <span class="db-label">
      <span class="db-label-name">{props.field.name}</span>
      <Show when={props.field.required}>
        <span class="db-required" title="Required" aria-label="required">
          *
        </span>
      </Show>
      <span class="muted small"> {props.field.kind === "list" ? `list of ${props.field.of ?? "string"}` : props.field.kind === "ref" ? `ref to ${props.field.ref}` : props.field.kind}</span>
      <Show when={props.field.description}>
        <span class="muted small db-desc">{props.field.description}</span>
      </Show>
    </span>
  );
}

/** One input for a field of any kind. */
export function FieldInput(props: { project: string; field: DbField; value: FormValue; onChange: (v: FormValue) => void }): JSX.Element {
  const text = () => (Array.isArray(props.value) ? props.value.join(",") : props.value);
  const f = () => props.field;
  return (
    <Switch>
      <Match when={f().kind === "bool"}>
        <label class="db-check">
          <input type="checkbox" checked={text() === "true"} onChange={(e) => props.onChange(e.currentTarget.checked ? "true" : "false")} />
          <FieldLabel field={f()} />
          <Show when={text() === "" && !f().required}>
            <span class="muted small">(no value)</span>
          </Show>
        </label>
      </Match>
      <Match when={f().kind === "list"}>
        <div class="field">
          <FieldLabel field={f()} />
          <Chips value={Array.isArray(props.value) ? props.value : []} kind={f().of ?? "string"} label={f().name} onChange={props.onChange} />
        </div>
      </Match>
      <Match when={f().kind === "ref"}>
        <div class="field">
          <FieldLabel field={f()} />
          <RefPicker project={props.project} target={f().ref ?? ""} value={text()} label={f().name} onChange={props.onChange} />
        </div>
      </Match>
      <Match when={f().kind === "text"}>
        <label>
          <FieldLabel field={f()} />
          <textarea rows={5} value={text()} onInput={(e) => props.onChange(e.currentTarget.value)} />
          <Show when={text().trim()}>
            <details class="db-preview">
              <summary class="small">Preview</summary>
              <div class="card-box">
                <Markdown html={renderMarkdown(text(), props.project)} />
              </div>
            </details>
          </Show>
        </label>
      </Match>
      <Match when={f().kind === "json"}>
        <JsonInput field={f()} value={text()} onChange={props.onChange} />
      </Match>
      <Match when={f().kind === "enum"}>
        <label>
          <FieldLabel field={f()} />
          <select value={text()} onChange={(e) => props.onChange(e.currentTarget.value)}>
            <option value="">{f().required ? "Pick a value" : "(no value)"}</option>
            <For each={f().values ?? []}>{(v) => <option value={v}>{v}</option>}</For>
          </select>
        </label>
      </Match>
      <Match when={true}>
        <label>
          <FieldLabel field={f()} />
          <input
            type={f().kind === "number" || f().kind === "integer" ? "number" : f().kind === "date" ? "date" : f().kind === "datetime" ? "datetime-local" : "text"}
            step={f().kind === "integer" ? "1" : f().kind === "number" ? "any" : undefined}
            maxLength={f().kind === "string" ? 1000 : undefined}
            value={text()}
            onInput={(e) => props.onChange(e.currentTarget.value)}
          />
        </label>
      </Match>
    </Switch>
  );
}

/** A JSON textarea that says at once whether the text parses. */
function JsonInput(props: { field: DbField; value: string; onChange: (v: string) => void }) {
  const problem = () => {
    if (!props.value.trim()) return null;
    try {
      JSON.parse(props.value);
      return null;
    } catch (e) {
      return (e as Error).message;
    }
  };
  return (
    <label>
      <FieldLabel field={props.field} />
      <textarea class="mono" rows={4} value={props.value} onInput={(e) => props.onChange(e.currentTarget.value)} />
      <Show when={problem()}>{(p) => <span class="field-error small">Not JSON: {p()}</span>}</Show>
    </label>
  );
}

/** A list editor: one chip for each item, and an input that adds one on Enter or `,`. */
function Chips(props: { value: string[]; kind: string; label: string; onChange: (v: string[]) => void }) {
  const [draft, setDraft] = createSignal("");
  const add = () => {
    const items = draft()
      .split(",")
      .map((x) => x.trim())
      .filter(Boolean);
    if (items.length) props.onChange([...props.value, ...items]);
    setDraft("");
  };
  return (
    <div class="db-chips">
      <For each={props.value}>
        {(item, i) => (
          <span class="chip db-chip">
            {item}
            <button type="button" class="ghost small" aria-label={`Remove ${item}`} onClick={() => props.onChange(props.value.filter((_, j) => j !== i()))}>
              ✕
            </button>
          </span>
        )}
      </For>
      <input
        type={props.kind === "string" ? "text" : "number"}
        placeholder="Add, then Enter"
        aria-label={`Add to ${props.label}`}
        value={draft()}
        onInput={(e) => setDraft(e.currentTarget.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === ",") {
            e.preventDefault();
            add();
          }
        }}
        onBlur={add}
      />
    </div>
  );
}

/**
 * A ref picker: search the records of the target type by its label field
 * (the first string field). The value is the record ID; the picker shows
 * the label.
 */
function RefPicker(props: { project: string; target: string; value: string; label: string; onChange: (v: string) => void }) {
  const { api } = useInstance();
  const [search, setSearch] = createSignal("");
  const [open, setOpen] = createSignal(false);
  const [target] = createResource(
    () => props.target || undefined,
    (t) => api.db.type(props.project, t).catch(() => null),
  );
  const labelField = () => target()?.fields.find((f) => f.kind === "string")?.name;
  // The label of the current value.
  const [current] = createResource(
    () => (props.value ? { id: props.value, lf: labelField() } : undefined),
    async (k) => {
      try {
        const r = await api.db.record(props.project, k.id);
        return k.lf ? String(r.data[k.lf] ?? k.id) : k.id;
      } catch {
        return k.id;
      }
    },
  );
  const [hits] = createResource(
    () => (open() && target() ? { s: search(), lf: labelField() } : undefined),
    async (k) => {
      const filter = k.s.trim() && k.lf ? ` ${k.lf}~${quoteValue(k.s.trim())}` : "";
      const sort = k.lf ? ` sort:${k.lf}` : "";
      const r = await api.db.query(props.project, `type:${props.target}${filter}${sort} limit:20`);
      return r.records.map((x) => ({ id: x.id, label: k.lf ? String(x.data[k.lf] ?? x.id) : x.id }));
    },
  );
  return (
    <div class="db-ref">
      <div class="row">
        <Show when={props.value} fallback={<span class="muted small">No {props.target} picked.</span>}>
          <span class="chip db-chip">
            {current() ?? props.value}
            <button type="button" class="ghost small" aria-label={`Clear ${props.label}`} onClick={() => props.onChange("")}>
              ✕
            </button>
          </span>
        </Show>
      </div>
      <input
        type="search"
        placeholder={`Search ${props.target}${labelField() ? ` by ${labelField()}` : ""}`}
        aria-label={`Search ${props.target} for ${props.label}`}
        value={search()}
        onFocus={() => setOpen(true)}
        onInput={(e) => {
          setSearch(e.currentTarget.value);
          setOpen(true);
        }}
      />
      <Show when={open()}>
        <ul class="db-ref-hits" role="listbox" aria-label={`${props.target} records`}>
          <Show when={!hits.loading} fallback={<li class="muted small">Searching…</li>}>
            <For each={hits() ?? []} fallback={<li class="muted small">No match.</li>}>
              {(h) => (
                <li>
                  <button
                    type="button"
                    class="ghost"
                    role="option"
                    aria-selected={h.id === props.value}
                    onClick={() => {
                      props.onChange(h.id);
                      setOpen(false);
                      setSearch("");
                    }}
                  >
                    {h.label}
                  </button>
                </li>
              )}
            </For>
          </Show>
        </ul>
      </Show>
    </div>
  );
}
