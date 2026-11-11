import { useInstance } from "../../instance";
// The login of an ssh, vnc, or rdp target (PLAN §16b.6a, docs/api.md
// §17.2): the secret name, the fingerprint of an ssh key, and the
// `credential_info.warning` when the credential does not work. Change sends
// `PATCH /api/targets/{name}` with the new `credential` (or `null`). Test
// login (ssh only) sends `POST /api/targets/{name}/test` and shows the
// result class, the message, the host key state, and the ssh stderr.

import { createSignal, Show } from "solid-js";
import type { Target, TargetTestReport } from "../../api/types";
import { ErrorBanner } from "../../components/ui";

import { CredentialSelect } from "../secrets/CredentialSelect";
import { credentialKind, GLOBAL, TEST_RESULT } from "../secrets/model";

/** A target whose kind takes a credential. */
export const takesCredential = (t: Pick<Target, "kind">) => credentialKind(t.kind) !== null;

/** The short cell of the target list: the key name and the fingerprint, with a warning mark. */
export function CredentialCell(props: { target: Target }) {
  const info = () => props.target.credential_info;
  return (
    <Show when={props.target.credential}>
      <div class="small" aria-label="Credential">
        <span class="muted">{props.target.kind === "ssh" ? "key" : "login"}</span> <span class="mono">{props.target.credential}</span>
        <Show when={info()?.fingerprint}>{(fp) => <span class="mono muted" title={fp()}> · {fp().slice(0, 19)}…</span>}</Show>
        <Show when={info()?.warning}>
          {(w) => (
            <>
              {" "}
              <span class="chip warn" title={w()}>
                (!) credential
              </span>
            </>
          )}
        </Show>
      </div>
    </Show>
  );
}

export function TargetCredential(props: { target: Target; onChanged: () => void; onNotice: (s: string) => void }) {
  const { api } = useInstance();
  const t = () => props.target;
  const [editing, setEditing] = createSignal(false);
  const [value, setValue] = createSignal(t().credential ?? "");
  const [busy, setBusy] = createSignal<"save" | "test" | null>(null);
  const [error, setError] = createSignal<unknown>(null);
  const [report, setReport] = createSignal<TargetTestReport | null>(null);
  const info = () => t().credential_info;
  const fileManaged = () => t().source === "file" || (t().source ?? "").startsWith("fleet:");

  const save = async () => {
    setBusy("save");
    try {
      await api.targets.update(t().name, { credential: value() || null });
      setError(null);
      setEditing(false);
      props.onNotice(value() ? `${t().name}: the login is now ${value()}.` : `${t().name}: no login.`);
      props.onChanged();
    } catch (e) {
      setError(e);
    } finally {
      setBusy(null);
    }
  };
  const test = async () => {
    setBusy("test");
    setReport(null);
    try {
      setReport(await api.targets.test(t().name));
      setError(null);
    } catch (e) {
      setError(e);
    } finally {
      setBusy(null);
    }
  };

  return (
    <div class="stack" style={{ gap: "6px" }} aria-label={`Login of ${t().name}`}>
      <div class="row" style={{ gap: "8px", "flex-wrap": "wrap" }}>
        <strong class="small">{t().kind === "ssh" ? "Key" : "Login"}</strong>
        <Show when={t().credential} fallback={<span class="small muted">none</span>}>
          <span class="mono small">{t().credential}</span>
          <Show when={info()?.scope}>{(s) => <span class={`chip ${s() === GLOBAL ? "info" : ""}`}>{s()}</span>}</Show>
          <Show when={info()?.fingerprint}>{(fp) => <code class="mono small">{fp()}</code>}</Show>
        </Show>
        <Show when={!editing()}>
          <button
            class="small"
            disabled={fileManaged()}
            title={fileManaged() ? "A file or fleet target: change it in its file" : undefined}
            onClick={() => {
              setValue(t().credential ?? "");
              setEditing(true);
            }}
          >
            Change
          </button>
        </Show>
        <Show when={t().kind === "ssh"}>
          <button class="small" disabled={busy() !== null} onClick={() => void test()}>
            {busy() === "test" ? "Testing…" : "Test login"}
          </button>
        </Show>
      </div>
      <Show when={info()?.warning}>
        {(w) => (
          <p class="error-banner small" role="alert" style={{ margin: 0 }}>
            {w()}
          </p>
        )}
      </Show>
      <Show when={editing()}>
        <div class="row" style={{ gap: "6px", "flex-wrap": "wrap", "align-items": "flex-start" }}>
          <div style={{ "min-width": "18em" }}>
            <CredentialSelect kind={t().kind} scope={t().project ?? GLOBAL} value={value()} targetName={t().name} onChange={setValue} />
          </div>
          <button class="primary small" disabled={busy() !== null} onClick={() => void save()}>
            {busy() === "save" ? "Saving…" : "Save"}
          </button>
          <button class="small" onClick={() => setEditing(false)}>
            Cancel
          </button>
        </div>
      </Show>
      <Show when={report()}>
        {(r) => (
          <div class="stack" style={{ gap: "2px" }} role="status" aria-label="Test result">
            <div class="row" style={{ gap: "6px", "flex-wrap": "wrap" }}>
              <span class={`chip ${TEST_RESULT[r().result]?.chip ?? ""}`}>{r().result}</span>
              <span class="small">{r().message}</span>
            </div>
            <span class="small muted">
              host key: {r().host_key}
              {r().host_key === "added" ? " (recorded at this first test: check the host)" : ""} · exit {r().exit_code ?? "—"} · {r().elapsed_ms} ms
            </span>
            <Show when={r().stderr}>
              <pre class="mono small" style={{ margin: 0, "white-space": "pre-wrap" }}>
                {r().stderr}
              </pre>
            </Show>
          </div>
        )}
      </Show>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
    </div>
  );
}
