import { useInstance } from "../instance";
// The login screen. It posts the operator password once to
// `POST /api/session`, or, on "Use the token instead", the bearer token. The
// daemon answers with an `HttpOnly` cookie; the page keeps only the CSRF
// value, in memory (stores/auth.ts). After a session ends it shows over the
// current page, so the route and the page state stay.

import { createSignal, onCleanup, Show } from "solid-js";
import { mockPassword, mockToken } from "../api";

import { loginError, type LoginProblem } from "../stores/auth";

export function LoginScreen(props: { overlay?: boolean }) {
  const { signInWithToken, signInPassword, auth } = useInstance();
  const [useToken, setUseToken] = createSignal(false);
  const [value, setValue] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [problem, setProblem] = createSignal<LoginProblem | null>(null);
  /** Seconds left of a `429` lockout. */
  const [wait, setWait] = createSignal(0);
  let timer: ReturnType<typeof setInterval> | undefined;
  const stopTimer = () => {
    if (timer !== undefined) clearInterval(timer);
    timer = undefined;
  };
  onCleanup(stopTimer);
  const countDown = (secs: number) => {
    stopTimer();
    setWait(secs);
    timer = setInterval(() => {
      setWait((w) => Math.max(0, w - 1));
      if (wait() === 0) stopTimer();
    }, 1000);
  };

  const submit = async () => {
    if (!value() || busy() || wait() > 0) return;
    setBusy(true);
    setProblem(null);
    try {
      if (useToken()) await signInWithToken(value());
      else await signInPassword(value());
      setValue("");
    } catch (e) {
      const p = loginError(e, useToken() ? "token" : "password");
      setProblem(p);
      if (p.retryAfter) countDown(p.retryAfter);
    } finally {
      setBusy(false);
    }
  };

  const switchMode = (token: boolean) => {
    setUseToken(token);
    setValue("");
    setProblem(null);
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
              <Show when={useToken()} fallback="Enter the operator password.">
                Enter the dashboard token. It is <code>TERRARIUM_TOKEN</code> in your <code>.env</code>.
              </Show>
            </p>
          }
        >
          <p style={{ margin: 0 }} role="status">
            <span class="chip warn">session ended</span> Sign in again to go on. <Show when={props.overlay}>You stay on this page.</Show>
          </p>
        </Show>
        <Show when={mockToken}>
          <p class="muted small" style={{ margin: 0 }}>
            Mock mode: the password is <code>{mockPassword}</code>, and the token is <code>{mockToken}</code>.
          </p>
        </Show>
        {/* A hidden user name helps password managers file the password. */}
        <input type="text" name="username" autocomplete="username" value="operator" readOnly hidden />
        <Show
          when={useToken()}
          fallback={
            <label>
              Password
              <input type="password" name="password" autocomplete="current-password" value={value()} onInput={(e) => setValue(e.currentTarget.value)} />
            </label>
          }
        >
          <label>
            Bearer token
            <input type="password" name="token" autocomplete="off" value={value()} onInput={(e) => setValue(e.currentTarget.value)} />
          </label>
        </Show>
        <p class="muted small" style={{ margin: 0 }}>
          The browser keeps an <code>HttpOnly</code> session cookie for {useToken() ? "the session" : "30 days"}, and renews it while you use the dashboard.
          This page keeps no password and no token.
        </p>
        <Show when={problem()}>
          {(p) => (
            <p class="small" role="alert" style={{ margin: 0 }}>
              <span class="chip bad">error</span> {wait() > 0 ? `Too many failed sign-ins. Try again in ${wait()} s.` : p().message}
            </p>
          )}
        </Show>
        <div class="row">
          <button class="primary" type="submit" disabled={busy() || wait() > 0}>
            {busy() ? "Signing in…" : "Sign in"}
          </button>
          <Show
            when={useToken()}
            fallback={
              <button type="button" class="ghost small" onClick={() => switchMode(true)}>
                Use the token instead
              </button>
            }
          >
            <button type="button" class="ghost small" onClick={() => switchMode(false)}>
              Use the password
            </button>
          </Show>
        </div>
      </form>
    </div>
  );
}
