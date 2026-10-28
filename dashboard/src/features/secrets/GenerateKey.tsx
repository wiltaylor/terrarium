// Generate an SSH key pair in the store (PLAN §16b.6a, docs/api.md §5.2
// `POST /api/secrets/{scope}/{name}/generate`). The private key never
// leaves the daemon; the answer shows the public key. The form is a group,
// not a <form>, so the target form can hold it too.

import { createSignal, For, Show } from "solid-js";
import { ApiError } from "../../api/http";
import type { Secret, SecretGenerate } from "../../api/types";
import { ErrorBanner } from "../../components/ui";
import { api } from "../../stores";
import { GLOBAL, NAME_RE } from "./model";
import { PublicKey } from "./PublicKey";

export function GenerateKeyForm(props: {
  /** The scope to start with: `global` or a project. */
  scope: string;
  /** The scopes to offer; one scope: no select. */
  scopes: readonly string[];
  name?: string;
  onGenerated: (s: Secret) => void;
  onCancel?: () => void;
  /** Show the public key of the new key in the form. */
  showResult?: boolean;
}) {
  const [scope, setScope] = createSignal(props.scope);
  const [name, setName] = createSignal(props.name ?? "");
  const [type, setType] = createSignal<"ed25519" | "rsa">("ed25519");
  const [bits, setBits] = createSignal<3072 | 4096>(3072);
  const [comment, setComment] = createSignal("");
  const [replace, setReplace] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const [exists, setExists] = createSignal(false);
  const [made, setMade] = createSignal<Secret | null>(null);

  const submit = async (e: Event) => {
    e.preventDefault();
    e.stopPropagation();
    const n = name().trim();
    if (!NAME_RE.test(n)) return setError(new Error("The name is 1-63 of a-z, 0-9, -, _."));
    const body: SecretGenerate = { type: type(), ...(type() === "rsa" ? { bits: bits() } : {}), ...(comment().trim() ? { comment: comment().trim() } : {}), ...(replace() ? { replace: true } : {}) };
    setBusy(true);
    try {
      const s = await api.secrets.generate(scope(), n, body);
      setError(null);
      setExists(false);
      setMade(s);
      props.onGenerated(s);
    } catch (err) {
      setExists(err instanceof ApiError && err.status === 409);
      setError(err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="stack" role="group" aria-label="Generate SSH key">
      <div class="form-grid">
        <label>
          Name
          <input value={name()} placeholder="deploy-key" required onInput={(e) => setName(e.currentTarget.value)} />
        </label>
        <Show when={props.scopes.length > 1}>
          <label>
            Scope
            <select aria-label="Key scope" value={scope()} onChange={(e) => setScope(e.currentTarget.value)}>
              <For each={props.scopes}>{(s) => <option value={s} selected={s === scope()}>{s === GLOBAL ? "Global" : `Project ${s}`}</option>}</For>
            </select>
          </label>
        </Show>
        <label>
          Type
          <select aria-label="Key type" value={type()} onChange={(e) => setType(e.currentTarget.value as "ed25519" | "rsa")}>
            <option value="ed25519">ed25519 (default)</option>
            <option value="rsa">rsa</option>
          </select>
        </label>
        <Show when={type() === "rsa"}>
          <label>
            Bits
            <select aria-label="Key bits" value={String(bits())} onChange={(e) => setBits(Number(e.currentTarget.value) as 3072 | 4096)}>
              <option value="3072">3072 (default)</option>
              <option value="4096">4096</option>
            </select>
          </label>
        </Show>
        <label>
          Comment
          <input value={comment()} placeholder={`terrarium:${scope()}/${name().trim() || "<name>"}`} onInput={(e) => setComment(e.currentTarget.value)} />
        </label>
      </div>
      <p class="small muted" style={{ margin: 0 }}>
        The daemon makes the key pair. The private key never leaves it; the store seals it with no passphrase.
      </p>
      <Show when={exists()}>
        <label class="row small">
          <input type="checkbox" checked={replace()} onChange={(e) => setReplace(e.currentTarget.checked)} />
          Replace the key {scope()}/{name().trim()}: the old key stops working
        </label>
      </Show>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={props.showResult && made()}>{(s) => <PublicKey secret={s()} />}</Show>
      <div class="row" style={{ "justify-content": "flex-end" }}>
        <Show when={props.onCancel}>
          <button type="button" onClick={() => props.onCancel?.()}>
            {made() ? "Close" : "Cancel"}
          </button>
        </Show>
        <button class="primary" type="button" disabled={busy()} onClick={submit}>
          {busy() ? "Generating…" : "Generate"}
        </button>
      </div>
    </div>
  );
}
