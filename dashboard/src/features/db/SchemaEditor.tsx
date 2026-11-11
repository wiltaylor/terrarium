import { useInstance } from "../../instance";
// The schema editor of the DB tab: make a type, or change the fields of one
// (add, rename, remove, kind, required, default, enum values, list items,
// ref target, unique). A removed field loses its data in every record, so
// the editor asks for a confirm in the page first. The daemon checks the
// records against the new schema; the records that fail show here.

import { createSignal, For, Index, Show } from "solid-js";
import type { DbType } from "../../api/types";
import { ErrorBanner, Modal } from "../../components/ui";

import { draftOf, emptyDraft, ITEM_KINDS, KINDS, schemaPatch, type FieldDraft } from "./model";

interface Failure {
  id: string;
  field: string;
  message: string;
}

export function SchemaEditor(props: { project: string; type?: DbType; typeNames: string[]; onClose: () => void; onSaved: (t: DbType) => void }) {
  const { api } = useInstance();
  const [name, setName] = createSignal(props.type?.name ?? "");
  const [description, setDescription] = createSignal(props.type?.description ?? "");
  const [rows, setRows] = createSignal<FieldDraft[]>(props.type ? props.type.fields.map(draftOf) : [{ ...emptyDraft(), name: "title", required: true }]);
  const [confirmed, setConfirmed] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const [problems, setProblems] = createSignal<string[]>([]);
  const [failures, setFailures] = createSignal<{ list: Failure[]; failing: number } | null>(null);
  const [busy, setBusy] = createSignal(false);

  const update = (i: number, patch: Partial<FieldDraft>) => setRows((r) => r.map((x, j) => (j === i ? { ...x, ...patch } : x)));
  const move = (i: number, by: number) =>
    setRows((r) => {
      const j = i + by;
      if (j < 0 || j >= r.length) return r;
      const next = [...r];
      [next[i], next[j]] = [next[j]!, next[i]!];
      return next;
    });
  /** The saved fields that the edit removes. */
  const removed = () => {
    const kept = new Set(rows().map((r) => r.orig));
    return (props.type?.fields ?? []).map((f) => f.name).filter((n) => !kept.has(n));
  };
  const targets = () => [...new Set([...props.typeNames, name()].filter(Boolean))].sort();

  const save = async () => {
    setError(null);
    setProblems([]);
    setFailures(null);
    const r = schemaPatch(props.type?.revision ?? 0, props.type?.fields ?? [], rows(), description());
    if (r.error) {
      setProblems([r.error]);
      return;
    }
    if (r.removed.length && !confirmed()) {
      setProblems([`Confirm first: the change removes ${r.removed.join(", ")} and their data.`]);
      return;
    }
    setBusy(true);
    try {
      const t = props.type
        ? await api.db.updateType(props.project, props.type.name, r.patch!)
        : await api.db.createType(props.project, { name: name().trim(), description: description(), fields: r.patch!.fields! });
      props.onSaved(t);
    } catch (e) {
      const d = (e as { details?: { problems?: string[]; records?: Failure[]; failing?: number } }).details;
      if (d?.problems) setProblems(d.problems);
      if (d?.records) setFailures({ list: d.records, failing: d.failing ?? d.records.length });
      // The lists above show the details; the banner shows the message only.
      setError(d?.problems || d?.records ? new Error((e as Error).message) : e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal title={props.type ? `Schema of ${props.type.name}` : "New type"} onClose={props.onClose} class="db-modal">
      <form
        class="stack"
        onSubmit={(e) => {
          e.preventDefault();
          void save();
        }}
      >
        <div class="form-grid">
          <label>
            Name
            <input value={name()} disabled={!!props.type} placeholder="bug" pattern="[a-z][a-z0-9_\-]{0,62}" onInput={(e) => setName(e.currentTarget.value)} />
          </label>
          <label>
            Description
            <input value={description()} onInput={(e) => setDescription(e.currentTarget.value)} />
          </label>
        </div>
        <Show when={props.type}>
          {(t) => (
            <p class="muted small" style={{ margin: 0 }}>
              Revision {t().revision} · {t().records} records. A rename moves the data. A new required field needs a default when the type has records. The daemon refuses a change that a record does not fit.
            </p>
          )}
        </Show>
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <Show when={problems().length}>
          <ul class="db-problems field-error small">
            <For each={problems()}>{(p) => <li>{p}</li>}</For>
          </ul>
        </Show>
        <Show when={failures()}>
          {(f) => (
            <div class="db-failures small">
              <strong>{f().failing} records do not fit the new schema:</strong>
              <ul>
                <For each={f().list}>
                  {(x) => (
                    <li>
                      <span class="mono">{x.id}</span>: {x.field} {x.message}
                    </li>
                  )}
                </For>
              </ul>
            </div>
          )}
        </Show>
        <div class="db-schema-rows">
          <Index each={rows()}>
            {(r, i) => (
              <fieldset class="db-schema-row">
                <legend class="small">
                  {r().name || "new field"}
                  <Show when={r().orig && r().orig !== r().name.trim()}>
                    <span class="muted"> (was {r().orig})</span>
                  </Show>
                </legend>
                <div class="form-grid">
                  <label>
                    Name
                    <input value={r().name} aria-label={`Field ${i + 1} name`} onInput={(e) => update(i, { name: e.currentTarget.value })} />
                  </label>
                  <label>
                    Kind
                    <select value={r().kind} aria-label={`Field ${i + 1} kind`} onChange={(e) => update(i, { kind: e.currentTarget.value as FieldDraft["kind"] })}>
                      <For each={KINDS}>{(k) => <option value={k}>{k}</option>}</For>
                    </select>
                  </label>
                  <Show when={r().kind === "enum"}>
                    <label>
                      Values (in sort order, comma separated)
                      <input value={r().values} placeholder="low,medium,high" onInput={(e) => update(i, { values: e.currentTarget.value })} />
                    </label>
                  </Show>
                  <Show when={r().kind === "list"}>
                    <label>
                      Items
                      <select value={r().of} onChange={(e) => update(i, { of: e.currentTarget.value as FieldDraft["of"] })}>
                        <For each={ITEM_KINDS}>{(k) => <option value={k}>{k}</option>}</For>
                      </select>
                    </label>
                  </Show>
                  <Show when={r().kind === "ref"}>
                    <label>
                      Target type
                      <select value={r().ref} onChange={(e) => update(i, { ref: e.currentTarget.value })}>
                        <option value="">Pick a type</option>
                        <For each={targets()}>{(t) => <option value={t}>{t}</option>}</For>
                      </select>
                    </label>
                  </Show>
                  <Show when={r().kind !== "ref"}>
                    <label>
                      Default
                      <input value={r().default} placeholder={r().kind === "list" ? "a,b" : r().kind === "datetime" ? "2026-10-01T09:00:00Z" : r().kind === "bool" ? "true or false" : ""} onInput={(e) => update(i, { default: e.currentTarget.value })} />
                    </label>
                  </Show>
                  <label>
                    Description
                    <input value={r().description} onInput={(e) => update(i, { description: e.currentTarget.value })} />
                  </label>
                </div>
                <div class="row">
                  <label class="db-check">
                    <input type="checkbox" checked={r().required} onChange={(e) => update(i, { required: e.currentTarget.checked })} /> Required
                  </label>
                  <label class="db-check">
                    <input type="checkbox" checked={r().unique} onChange={(e) => update(i, { unique: e.currentTarget.checked })} /> Unique
                  </label>
                  <span style={{ flex: 1 }} />
                  <button type="button" class="small" aria-label={`Move ${r().name} up`} disabled={i === 0} onClick={() => move(i, -1)}>
                    ↑
                  </button>
                  <button type="button" class="small" aria-label={`Move ${r().name} down`} disabled={i === rows().length - 1} onClick={() => move(i, 1)}>
                    ↓
                  </button>
                  <button type="button" class="small danger" onClick={() => setRows((x) => x.filter((_, j) => j !== i))}>
                    Remove field
                  </button>
                </div>
              </fieldset>
            )}
          </Index>
        </div>
        <div>
          <button type="button" onClick={() => setRows((r) => [...r, emptyDraft()])}>
            Add field
          </button>
        </div>
        <Show when={removed().length}>
          <label class="db-check db-confirm">
            <input type="checkbox" checked={confirmed()} onChange={(e) => setConfirmed(e.currentTarget.checked)} />
            <span>
              Remove <strong>{removed().join(", ")}</strong> and the data of these fields from all {props.type?.records ?? 0} records.
            </span>
          </label>
        </Show>
        <div class="row">
          <button type="submit" class="primary" disabled={busy() || (!props.type && !name().trim())}>
            {props.type ? "Save schema" : "Create type"}
          </button>
          <button type="button" onClick={props.onClose}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}
