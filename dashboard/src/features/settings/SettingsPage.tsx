// Settings: the sign-in session. How the dashboard authenticates, when the
// cookie session ends (`expires_at` of `POST /api/session`), and what this
// browser keeps (src/README.md, "Authentication").

import { Match, Show, Switch } from "solid-js";
import { PageHead, useNow } from "../../components/ui";
import { timeUntil } from "../../lib/format";
import { auth, signOutNow, theme } from "../../stores";

export default function SettingsPage() {
  const now = useNow(30_000);
  const ends = () => {
    const at = auth.expiresAt();
    if (!at) return null;
    const ms = Date.parse(at);
    return Number.isFinite(ms) ? { at, local: new Date(ms).toLocaleString(), left: timeUntil(at, now()) } : { at, local: at, left: "" };
  };

  return (
    <div class="stack">
      <PageHead title="Settings" />

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
                    <Show when={e().left}> (in {e().left})</Show>
                  </span>
                )}
              </Show>
            </dd>
            <dt>The token</dt>
            <dd>Not in this browser. It went to the daemon once, at sign-in.</dd>
            <dt>The cookie</dt>
            <dd>
              <code>terrarium_session</code>, <code>HttpOnly</code> and <code>SameSite=Strict</code>. Script on this page cannot read it. The
              browser drops it when the session ends.
            </dd>
            <dt>This browser keeps</dt>
            <dd>
              The CSRF value, in memory and in this tab's session storage
              <Show when={auth.remembered()} fallback=". A new tab asks you to sign in again.">
                , and in local storage (<strong>Remember on this device</strong>). A new tab stays signed in until the session ends.
              </Show>
            </dd>
          </Show>

          <Show when={auth.mode() === "bearer"}>
            <dt>Why</dt>
            <dd>This daemon has no cookie sessions (<code>POST /api/session</code> answered 404).</dd>
            <dt>This browser keeps</dt>
            <dd>
              The bearer token, in memory and in this tab's session storage
              <Show when={auth.remembered()} fallback=".">, and in local storage (Remember on this device).</Show> Any script on this page can
              read it. Update the daemon to get cookie sessions; the next load trades the token for a session.
            </dd>
          </Show>
        </dl>
        <div class="row">
          <button onClick={() => void signOutNow()}>Sign out</button>
        </div>
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
