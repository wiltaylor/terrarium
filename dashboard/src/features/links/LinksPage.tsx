// Settings, Links tab (docs/api.md §36, TER-61): the link keys with which
// another Terrarium daemon (the home) shows and runs this one. "New link"
// makes a one-time pairing code (10 minutes) that the operator types on
// the home; the home swaps it for the long-lived key, so the key never shows
// here. Each row has the home, the key use record, Rotate (a new code, the
// old key stops now), and Revoke (the key stops, its streams close).

import { createResource, createSignal, For, Show } from "solid-js";
import { ConfirmButton, CopyButton, Empty, ErrorBanner, Modal, PageHead, useNow } from "../../components/ui";
import type { NewLinkCode } from "../../api/types";
import { ago } from "../../lib/format";
import { api } from "../../stores";
import { linkChip } from "./model";

export default function LinksPage() {
  const now = useNow(30_000);
  const [showRevoked, setShowRevoked] = createSignal(false);
  const [links, { refetch }] = createResource(showRevoked, (revoked) => api.links.list({ revoked }));
  const [error, setError] = createSignal<unknown>(null);
  const [adding, setAdding] = createSignal(false);
  // The code of a rotate: shown in the same dialog as a new link.
  const [rotated, setRotated] = createSignal<NewLinkCode | null>(null);

  const act = async (f: () => Promise<unknown>) => {
    try {
      await f();
      setError(null);
      void refetch();
    } catch (e) {
      setError(e);
    }
  };

  return (
    <>
      <PageHead title="Links" level={2}>
        <label class="small row" style={{ gap: "4px" }}>
          <input type="checkbox" checked={showRevoked()} onChange={(e) => setShowRevoked(e.currentTarget.checked)} />
          Show revoked
        </label>
        <button onClick={() => void refetch()}>Refresh</button>
        <button class="primary" onClick={() => setAdding(true)}>
          New link
        </button>
      </PageHead>
      <p class="small muted">
        A link lets another Terrarium daemon (the home) show this one in its dashboard and act on it as the operator, except for the credentials, the backups, the
        update, and the config repo push. A read-only link only reads.
      </p>
      <ErrorBanner error={error() ?? links.error} onDismiss={() => setError(null)} />
      <section class="card-box">
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Link</th>
                <th>Status</th>
                <th>Home</th>
                <th>Last used</th>
                <th />
              </tr>
            </thead>
            <tbody>
              <For each={links.latest} fallback={<tr><td colSpan={5}><Empty>{links.loading ? "Loading…" : "No links. Make one, then add this daemon as a remote on the home."}</Empty></td></tr>}>
                {(l) => (
                  <tr>
                    <td>
                      <strong>{l.label}</strong>
                      <Show when={l.read_only}>
                        {" "}
                        <span class="chip">read-only</span>
                      </Show>
                      <div class="small muted mono">{l.id}</div>
                    </td>
                    <td>
                      <span class={`chip ${linkChip(l)}`}>{l.status}</span>
                      <Show when={l.status === "pending" && l.pair_expires_at}>
                        {(at) => <div class="small muted">code until {new Date(at()).toLocaleTimeString()}</div>}
                      </Show>
                    </td>
                    <td class="small">
                      <Show when={l.home_instance_id} fallback={<span class="muted">not paired</span>}>
                        {l.home_name ?? "—"} <span class="mono muted">{l.home_instance_id}</span>
                        <Show when={l.paired_from}>
                          <div class="muted">paired from {l.paired_from}</div>
                        </Show>
                      </Show>
                    </td>
                    <td class="small">
                      {l.last_used_at ? ago(l.last_used_at, now()) : "never"}
                      <Show when={l.last_used_from}>
                        <div class="muted">from {l.last_used_from}</div>
                      </Show>
                    </td>
                    <td>
                      <Show when={l.status !== "revoked"}>
                        <span class="row" style={{ gap: "4px" }}>
                          <ConfirmButton
                            class="small"
                            label="Rotate"
                            confirmLabel="Yes, stop the key and make a new code"
                            onConfirm={() =>
                              act(async () => {
                                setRotated(await api.links.rotate(l.id));
                              })
                            }
                          />
                          <ConfirmButton class="small danger" label="Revoke" confirmLabel="Yes, revoke the link" onConfirm={() => act(() => api.links.revoke(l.id))} />
                        </span>
                      </Show>
                    </td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
      </section>
      <Show when={adding()}>
        <NewLinkDialog
          onClose={() => {
            setAdding(false);
            void refetch();
          }}
        />
      </Show>
      <Show when={rotated()}>
        {(m) => (
          <Modal title="New pairing code" onClose={() => setRotated(null)}>
            <PairingCode made={m()} onDone={() => setRotated(null)} />
          </Modal>
        )}
      </Show>
    </>
  );
}

function NewLinkDialog(props: { onClose: () => void }) {
  const [label, setLabel] = createSignal("");
  const [readOnly, setReadOnly] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  // The code lives in this dialog only. Closing it drops the code for good.
  const [made, setMade] = createSignal<NewLinkCode | null>(null);

  const submit = async (e: Event) => {
    e.preventDefault();
    try {
      setMade(await api.links.create(label().trim(), { readOnly: readOnly() }));
      setError(null);
    } catch (err) {
      setError(err);
    }
  };

  return (
    <Modal title="New link" onClose={() => props.onClose()}>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show
        when={made()}
        fallback={
          <form class="stack" onSubmit={submit}>
            <label>
              Label (for example the name of the home)
              <input value={label()} onInput={(e) => setLabel(e.currentTarget.value)} placeholder="office" required autocomplete="off" maxLength={64} />
            </label>
            <label class="row" style={{ gap: "6px" }}>
              <input type="checkbox" checked={readOnly()} onChange={(e) => setReadOnly(e.currentTarget.checked)} />
              Read-only: the home can look, but not start agents, send turns, open terminals, or change anything
            </label>
            <div>
              <button class="primary" type="submit" disabled={!label().trim()}>
                Make pairing code
              </button>
            </div>
          </form>
        }
      >
        {(m) => <PairingCode made={m()} onDone={() => props.onClose()} />}
      </Show>
    </Modal>
  );
}

function PairingCode(props: { made: NewLinkCode; onDone: () => void }) {
  const m = () => props.made;
  return (
    <div class="stack">
      <p class="notice" style={{ margin: 0 }}>
        On the home, add a remote with this daemon's URL and this code. It shows <strong>once</strong> and works one time, until{" "}
        {new Date(m().code_expires_at).toLocaleTimeString()}. The daemon keeps only a hash.
      </p>
      <div class="copy-row">
        <code class="token-once" aria-label="Pairing code">
          {m().code}
        </code>
        <CopyButton text={m().code} label="Copy code" />
      </div>
      <dl class="small">
        <dt>URL</dt>
        <dd class="mono">{m().url ?? "this daemon's https URL (set TERRARIUM_PUBLIC_URL to show it here)"}</dd>
        <dt>Instance</dt>
        <dd>
          <span class="mono">{m().instance.id}</span> ({m().instance.name}, {m().instance.version})
        </dd>
      </dl>
      <p class="small muted" style={{ margin: 0 }}>
        The home then shows the instance ID and the TLS certificate fingerprint it saw. Compare them with the instance above and with the certificate of your
        reverse proxy before you confirm.
      </p>
      <div>
        <button class="primary" onClick={() => props.onDone()}>
          Done
        </button>
      </div>
    </div>
  );
}
