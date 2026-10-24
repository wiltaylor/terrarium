import { createSignal } from "solid-js";
import { auth } from "../stores";

export function TokenScreen() {
  const [value, setValue] = createSignal("");
  return (
    <div class="token-screen">
      <form
        class="card-box stack"
        onSubmit={(e) => {
          e.preventDefault();
          if (value().trim()) {
            auth.setToken(value());
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
        <div class="row">
          <button class="primary" type="submit">
            Save token
          </button>
        </div>
      </form>
    </div>
  );
}
