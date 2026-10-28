// Reveal (PLAN §16b.5, docs/api.md §5.2): the operator only, with the
// operator password, asked in the page. The values show for 30 seconds,
// then go. They live in this panel only: never in a store, the URL, or
// storage. A close of the panel drops them at once.

import { createSignal, For, onCleanup, Show } from "solid-js";
import type { Secret } from "../../api/types";
import { CopyButton } from "../../components/ui";
import { api } from "../../stores";
import { createRevealHold, revealError } from "./model";

export function RevealPanel(props: { secret: Secret; onClose: () => void }) {
  const [password, setPassword] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [shown, setShown] = createSignal<{ values: Record<string, string>; left: number } | null>(null);
  const hold = createRevealHold(setShown);
  onCleanup(() => hold.hide());

  const submit = async (e: Event) => {
    e.preventDefault();
    const pw = password();
    // The password goes in the one request, then out of the page.
    setPassword("");
    setBusy(true);
    try {
      const r = await api.secrets.reveal(props.secret.scope, props.secret.name, pw);
      setError(null);
      hold.show(r.values);
    } catch (err) {
      setError(revealError(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="stack" style={{ gap: "6px" }} role="region" aria-label={`Reveal panel of ${props.secret.scope}/${props.secret.name}`}>
      <Show
        when={shown()}
        fallback={
          <form class="row" style={{ gap: "6px", "flex-wrap": "wrap" }} onSubmit={submit} autocomplete="off">
            <label class="row small">
              Operator password
              <input type="password" aria-label="Operator password" autocomplete="current-password" value={password()} required onInput={(e) => setPassword(e.currentTarget.value)} />
            </label>
            <button class="primary small" type="submit" disabled={busy() || !password()}>
              {busy() ? "Checking…" : "Reveal"}
            </button>
            <button class="small" type="button" onClick={() => props.onClose()}>
              Cancel
            </button>
            <span class="small muted">Each reveal writes an audit row. The values show for 30 seconds.</span>
          </form>
        }
      >
        {(s) => (
          <>
            <div class="spread">
              <strong class="small">Values of {props.secret.scope}/{props.secret.name}</strong>
              <span class="row small" style={{ gap: "6px" }}>
                <span class="muted" role="timer" aria-label="Seconds left">
                  hides in {s().left} s
                </span>
                <button
                  class="small"
                  type="button"
                  onClick={() => {
                    hold.hide();
                    props.onClose();
                  }}
                >
                  Hide
                </button>
              </span>
            </div>
            <For each={Object.entries(s().values)}>
              {([k, v]) => (
                <div class="row" style={{ gap: "6px", "align-items": "flex-start" }}>
                  <span class="mono small muted" style={{ "min-width": "8em" }}>
                    {k}
                  </span>
                  <pre class="mono small" style={{ margin: 0, "white-space": "pre-wrap", "word-break": "break-all", flex: "1" }}>
                    {v}
                  </pre>
                  <CopyButton text={v} />
                </div>
              )}
            </For>
          </>
        )}
      </Show>
      <Show when={error()}>
        {(m) => (
          <p class="error-banner small" role="alert" style={{ margin: 0 }}>
            {m()}
          </p>
        )}
      </Show>
    </div>
  );
}
