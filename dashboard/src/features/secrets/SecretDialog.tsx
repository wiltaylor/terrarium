// New secret and Replace value (docs/api.md §5.2): a form for each kind.
// A `generic` secret has rows; each row is secret or plain. The values live
// in this dialog only: a save clears them, and a close drops them.

import { createSignal, For, Index, Show } from "solid-js";
import type { Secret, SecretKind } from "../../api/types";
import { SECRET_KINDS } from "../../api/types";
import { ErrorBanner, Modal } from "../../components/ui";
import { api } from "../../stores";
import { createBody, emptyForm, GLOBAL, KINDS, putBody, replaceForm, type SecretForm } from "./model";
import { SecretInput } from "./SecretInput";

export function SecretDialog(props: {
  /** Replace the value of this secret; else a new secret. */
  secret?: Secret;
  /** The scope of a new secret. */
  scope: string;
  /** The scopes to offer for a new secret. */
  scopes: readonly string[];
  onClose: () => void;
  onSaved: (s: Secret) => void;
}) {
  const start = () => (props.secret ? replaceForm(props.secret) : emptyForm(props.scope));
  const [form, setForm] = createSignal<SecretForm>(start());
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const f = form;
  const set = (patch: Partial<SecretForm>) => setForm((x) => ({ ...x, ...patch }));
  const setValue = (k: string, v: string) => setForm((x) => ({ ...x, values: { ...x.values, [k]: v } }));
  const setRow = (i: number, patch: Partial<SecretForm["rows"][number]>) => setForm((x) => ({ ...x, rows: x.rows.map((r, j) => (j === i ? { ...r, ...patch } : r)) }));
  const replacing = () => !!props.secret;

  const submit = async (e: Event) => {
    e.preventDefault();
    setBusy(true);
    try {
      let saved: Secret;
      if (props.secret) {
        const b = putBody(f());
        if ("error" in b) throw new Error(b.error);
        saved = await api.secrets.put(props.secret.scope, props.secret.name, b.body);
      } else {
        const b = createBody(f());
        if ("error" in b) throw new Error(b.error);
        saved = await api.secrets.create(b.body);
      }
      // Clear the inputs: no value stays in the page after a save.
      setForm(emptyForm(props.scope));
      setError(null);
      props.onSaved(saved);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal title={replacing() ? `Replace value · ${props.secret!.scope}/${props.secret!.name}` : "New secret"} onClose={() => props.onClose()}>
      <form class="stack" aria-label={replacing() ? "Replace value" : "New secret"} onSubmit={submit} autocomplete="off">
        <div class="form-grid">
          <label>
            Name
            <input value={f().name} required disabled={replacing()} placeholder="github-deploy" onInput={(e) => set({ name: e.currentTarget.value })} />
          </label>
          <label>
            Scope
            <select aria-label="Scope" value={f().scope} disabled={replacing()} onChange={(e) => set({ scope: e.currentTarget.value })}>
              <For each={replacing() ? [f().scope] : props.scopes}>
                {(s) => (
                  <option value={s} selected={s === f().scope}>
                    {s === GLOBAL ? "Global (sysadmin or the operator)" : `Project ${s}`}
                  </option>
                )}
              </For>
            </select>
          </label>
          <label>
            Kind
            <select aria-label="Kind" value={f().kind} disabled={replacing()} onChange={(e) => set({ kind: e.currentTarget.value as SecretKind, values: {} })}>
              <For each={SECRET_KINDS}>
                {(k) => (
                  <option value={k} selected={k === f().kind}>
                    {KINDS[k].label} ({k})
                  </option>
                )}
              </For>
            </select>
          </label>
        </div>
        <p class="small muted" style={{ margin: 0 }}>
          {KINDS[f().kind].help}
          {replacing() ? " A replace writes every field again: give each secret field its new value." : ""}
        </p>
        <Show when={f().kind !== "generic"}>
          <div class="form-grid">
            <For each={KINDS[f().kind].fields}>
              {(spec) => (
                <label style={spec.multiline ? { "grid-column": "1 / -1" } : undefined}>
                  {spec.label}
                  {spec.required ? "" : " (optional)"}
                  <Show
                    when={spec.secret}
                    fallback={<input value={f().values[spec.name] ?? ""} placeholder={spec.placeholder} onInput={(e) => setValue(spec.name, e.currentTarget.value)} />}
                  >
                    <SecretInput label={spec.label} value={f().values[spec.name] ?? ""} multiline={spec.multiline} placeholder={spec.placeholder} required={spec.required} onInput={(v) => setValue(spec.name, v)} />
                  </Show>
                </label>
              )}
            </For>
          </div>
        </Show>
        <Show when={f().kind === "generic"}>
          <div class="table-wrap">
            <table aria-label="Fields">
              <thead>
                <tr>
                  <th>Field</th>
                  <th>Value</th>
                  <th>Secret</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                <Index each={f().rows}>
                  {(r, i) => (
                    <tr>
                      <td>
                        <input aria-label={`Field ${i + 1} name`} value={r().key} placeholder="password" onInput={(e) => setRow(i, { key: e.currentTarget.value })} />
                      </td>
                      <td>
                        <Show when={r().secret} fallback={<input aria-label={`Field ${i + 1} value`} value={r().value} onInput={(e) => setRow(i, { value: e.currentTarget.value })} />}>
                          <SecretInput label={`Field ${i + 1} value`} value={r().value} onInput={(v) => setRow(i, { value: v })} />
                        </Show>
                      </td>
                      <td>
                        <label class="row small">
                          <input type="checkbox" checked={r().secret} onChange={(e) => setRow(i, { secret: e.currentTarget.checked })} />
                          {r().secret ? "secret" : "plain"}
                        </label>
                      </td>
                      <td>
                        <button type="button" class="small ghost" aria-label={`Remove field ${i + 1}`} disabled={f().rows.length === 1} onClick={() => set({ rows: f().rows.filter((_, j) => j !== i) })}>
                          Remove
                        </button>
                      </td>
                    </tr>
                  )}
                </Index>
              </tbody>
            </table>
          </div>
          <div>
            <button type="button" class="small" disabled={f().rows.length >= 32} onClick={() => set({ rows: [...f().rows, { key: "", value: "", secret: true }] })}>
              Add field
            </button>
          </div>
        </Show>
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <div class="row" style={{ "justify-content": "flex-end" }}>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
          <button class="primary" type="submit" disabled={busy()}>
            {busy() ? "Saving…" : replacing() ? "Replace" : "Create"}
          </button>
        </div>
      </form>
    </Modal>
  );
}
