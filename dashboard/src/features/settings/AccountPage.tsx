// Settings, Account tab: the sign-in session and the operator login. How the dashboard
// authenticates, when the cookie session ends (it renews while in use), the
// password, and the two log-outs (src/README.md, "Authentication").

import { createSignal, Match, onMount, Show, Switch } from "solid-js";
import { ConfirmButton, ErrorBanner, PageHead, useNow } from "../../components/ui";
import { timeUntil } from "../../lib/format";
import { api, auth, changePasswordNow, signOutAll, signOutNow, theme } from "../../stores";
import { ApiError } from "../../api";
import { MIN_PASSWORD, passwordFormError } from "../../stores/auth";

export default function AccountPage() {
  const now = useNow(30_000);
  const ends = () => {
    const at = auth.expiresAt();
    if (!at) return null;
    const ms = Date.parse(at);
    return Number.isFinite(ms) ? { at, local: new Date(ms).toLocaleString(), left: timeUntil(at, now()) } : { at, local: at, left: "" };
  };
  // The cookie renews while in use: ask for the current end.
  onMount(() => {
    if (auth.mode() !== "session") return;
    void api.session
      .current()
      .then((info) => {
        if (info) {
          auth.setPasswordSet(info.password_set);
          auth.startSession({ csrf: info.csrf ?? auth.csrf(), expires_at: info.expires_at ?? null });
        }
      })
      .catch(() => undefined);
  });

  const [current, setCurrent] = createSignal("");
  const [next, setNext] = createSignal("");
  const [again, setAgain] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [formError, setFormError] = createSignal<string | null>(null);
  const [error, setError] = createSignal<unknown>(null);
  const [done, setDone] = createSignal(false);
  // A cookie session must send the current password when one is set.
  const needCurrent = () => auth.mode() === "session" && auth.passwordSet() !== false;

  const submit = async () => {
    setDone(false);
    setError(null);
    const bad = passwordFormError({ current: current(), next: next(), again: again(), needCurrent: needCurrent() });
    setFormError(bad);
    if (bad || busy()) return;
    setBusy(true);
    try {
      await changePasswordNow(current(), next());
      setCurrent("");
      setNext("");
      setAgain("");
      setDone(true);
    } catch (e) {
      if (e instanceof ApiError && e.status === 403) setFormError("The current password is not correct.");
      else setError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="stack">
      <PageHead title="Account" level={2} />

      <section class="card-box stack" aria-labelledby="session-h">
        <h2 id="session-h" style={{ margin: 0 }}>
          Session
        </h2>
        <dl class="session-facts">
          <dt>Signed in with</dt>
          <dd>
            <Switch>
              <Match when={auth.mode() === "session"}>
                <span class="chip ok">cookie session</span>
              </Match>
              <Match when={auth.mode() === "bearer"}>
                <span class="chip warn">bearer token</span>
              </Match>
              <Match when={true}>
                <span class="chip">signed out</span>
              </Match>
            </Switch>
          </dd>

          <Show when={auth.mode() === "session"}>
            <dt>Session ends</dt>
            <dd>
              <Show when={ends()} fallback="unknown">
                {(e) => (
                  <span title={e().at}>
                    {e().local}
                    <Show when={e().left}> (in {e().left})</Show>. The daemon renews the cookie while you use the dashboard.
                  </span>
                )}
              </Show>
            </dd>
            <dt>The cookie</dt>
            <dd>
              <code>terrarium_session</code>, a signed token, <code>HttpOnly</code> and <code>SameSite=Strict</code>. Script on this page cannot
              read it. It stays through a browser restart until it ends.
            </dd>
            <dt>This browser keeps</dt>
            <dd>The CSRF value, in memory only. At each load the page asks the daemon for it again. No password and no token.</dd>
          </Show>

          <Show when={auth.mode() === "bearer"}>
            <dt>Why</dt>
            <dd>This daemon has no cookie sessions (<code>POST /api/session</code> answered 404).</dd>
            <dt>This browser keeps</dt>
            <dd>
              The bearer token, in memory and in this tab's session storage. Any script on this page can read it. Update the daemon to get cookie
              sessions; the next load trades the token for a session.
            </dd>
          </Show>
        </dl>
        <div class="row">
          <button onClick={() => void signOutNow()}>Log out</button>
          <Show when={auth.mode() === "session"}>
            <ConfirmButton label="Log out everywhere" confirmLabel="Yes, end every session" onConfirm={() => signOutAll().catch((e: unknown) => void setError(e))} />
          </Show>
        </div>
        <p class="muted small" style={{ margin: 0 }}>
          Log out everywhere ends every dashboard and CLI session (<code>terrarium login</code>). The <code>TERRARIUM_TOKEN</code> stays good.
        </p>
      </section>

      <section class="card-box stack" aria-labelledby="password-h">
        <h2 id="password-h" style={{ margin: 0 }}>
          Operator password
        </h2>
        <Show when={auth.passwordSet() === false}>
          <p class="small" style={{ margin: 0 }}>
            <span class="chip warn">not set</span> Set a password to sign in without the token. You can also run <code>terrariumd password set</code> on
            the daemon host.
          </p>
        </Show>
        <form
          class="stack"
          onSubmit={(e) => {
            e.preventDefault();
            void submit();
          }}
        >
          {/* A hidden user name helps password managers file the password. */}
          <input type="text" name="username" autocomplete="username" value="operator" readOnly hidden />
          <Show when={needCurrent()}>
            <label>
              Current password
              <input type="password" autocomplete="current-password" value={current()} onInput={(e) => setCurrent(e.currentTarget.value)} />
            </label>
          </Show>
          <label>
            New password
            <input type="password" autocomplete="new-password" value={next()} onInput={(e) => setNext(e.currentTarget.value)} />
          </label>
          <label>
            New password again
            <input type="password" autocomplete="new-password" value={again()} onInput={(e) => setAgain(e.currentTarget.value)} />
          </label>
          <p class="muted small" style={{ margin: 0 }}>
            At least {MIN_PASSWORD} characters. The change ends every session; this page signs in again with the new password.
          </p>
          <Show when={formError()}>
            <p class="small" role="alert" style={{ margin: 0 }}>
              <span class="chip bad">error</span> {formError()}
            </p>
          </Show>
          <Show when={error()}>
            <ErrorBanner error={error()} onDismiss={() => setError(null)} />
          </Show>
          <Show when={done()}>
            <p class="small" role="status" style={{ margin: 0 }}>
              <span class="chip ok">done</span> The password changed. Every other session has ended.
            </p>
          </Show>
          <div class="row">
            <button class="primary" type="submit" disabled={busy()}>
              {busy() ? "Saving…" : auth.passwordSet() === false ? "Set password" : "Change password"}
            </button>
          </div>
        </form>
      </section>

      <section class="card-box stack" aria-labelledby="look-h">
        <h2 id="look-h" style={{ margin: 0 }}>
          Look
        </h2>
        <div class="row">
          <button class="small" onClick={() => theme.cycle()}>
            Theme: {theme.pref()}
          </button>
        </div>
      </section>
    </div>
  );
}
