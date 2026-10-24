import { createSignal, Show } from "solid-js";
import { auth } from "../stores";

export function TokenScreen() {
  const [value, setValue] = createSignal("");
  const [remember, setRemember] = createSignal(auth.remembered());
  return (
    <div class="token-screen">
      <form
        class="card-box stack"
        onSubmit={(e) => {
          e.preventDefault();
          if (value().trim()) {
            auth.setToken(value(), remember());
            location.reload();
          }
        }}
      >
        <h1>Terrarium</h1>
        <p class="muted" style={{ margin: 0 }}>
          The daemon refused the request. Enter the dashboard token. It is <code>TERRARIUM_TOKEN</code> in your <code>.env</code>.
        </p>
        <label>
          Bearer token
          <input type="password" autocomplete="current-password" value={value()} onInput={(e) => setValue(e.currentTarget.value)} />
        </label>
        <label class="row small">
          <input type="checkbox" checked={remember()} onChange={(e) => setRemember(e.currentTarget.checked)} /> Remember on this device
        </label>
        <Show
          when={remember()}
          fallback={<p class="muted small" style={{ margin: 0 }}>The token stays in this tab only. A new tab asks again.</p>}
        >
          <p class="small" role="alert" style={{ margin: 0 }}>
            <span class="chip warn">warning</span> The token goes into this browser's local storage and stays after you close the tab. Anyone
            who uses this browser profile, and any script that runs on this page, can read it. Do not tick this on a shared computer.
          </p>
        </Show>
        <div class="row">
          <button class="primary" type="submit">
            Save token
          </button>
        </div>
      </form>
    </div>
  );
}
