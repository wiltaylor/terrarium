// The sign-in screen. It posts the token once to `POST /api/session` and
// keeps only the CSRF value (stores/auth.ts). After a session ends it shows
// over the current page, so the route and the page state stay.

import { createSignal, Show } from "solid-js";
import { ApiError, mockToken } from "../api";
import { auth, signInWithToken } from "../stores";

export function TokenScreen(props: { overlay?: boolean }) {
  const [value, setValue] = createSignal("");
  const [remember, setRemember] = createSignal(auth.remembered());
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const submit = async () => {
    if (!value().trim() || busy()) return;
    setBusy(true);
    setError(null);
    try {
      await signInWithToken(value(), remember());
      setValue("");
    } catch (e) {
      setError(
        e instanceof ApiError && e.status === 401
          ? "The daemon refused the token."
          : `Sign-in failed: ${e instanceof Error ? e.message : String(e)}`,
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class={`token-screen${props.overlay ? " overlay" : ""}`} role={props.overlay ? "dialog" : undefined} aria-modal={props.overlay ? "true" : undefined}>
      <form
        class="card-box stack"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <h1>Terrarium</h1>
        <Show
          when={auth.expired()}
          fallback={
            <p class="muted" style={{ margin: 0 }}>
              Enter the dashboard token. It is <code>TERRARIUM_TOKEN</code> in your <code>.env</code>.
            </p>
          }
        >
          <p style={{ margin: 0 }} role="status">
            <span class="chip warn">session ended</span> Sign in again to go on. <Show when={props.overlay}>You stay on this page.</Show>
          </p>
        </Show>
        <Show when={mockToken}>
          <p class="muted small" style={{ margin: 0 }}>
            Mock mode: the token is <code>{mockToken}</code>.
          </p>
        </Show>
        <label>
          Bearer token
          <input type="password" autocomplete="current-password" value={value()} onInput={(e) => setValue(e.currentTarget.value)} />
        </label>
        <label class="row small">
          <input type="checkbox" checked={remember()} onChange={(e) => setRemember(e.currentTarget.checked)} /> Remember on this device
        </label>
        <Show
          when={remember()}
          fallback={
            <p class="muted small" style={{ margin: 0 }}>
              The token goes to the daemon once. The browser keeps an <code>HttpOnly</code> session cookie, and this tab keeps the CSRF value. A new
              tab asks again.
            </p>
          }
        >
          <p class="small" role="alert" style={{ margin: 0 }}>
            <span class="chip warn">warning</span> The CSRF value goes into this browser's local storage, so a new tab stays signed in until the
            session ends. The token is not stored. If the daemon has no cookie sessions, the token itself goes into local storage, and any script
            on this page can read it. Do not tick this on a shared computer.
          </p>
        </Show>
        <Show when={error()}>
          <p class="small" role="alert" style={{ margin: 0 }}>
            <span class="chip bad">error</span> {error()}
          </p>
        </Show>
        <div class="row">
          <button class="primary" type="submit" disabled={busy()}>
            {busy() ? "Signing in…" : "Sign in"}
          </button>
        </div>
      </form>
    </div>
  );
}

