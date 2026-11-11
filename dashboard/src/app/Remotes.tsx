import { A } from "@solidjs/router";
import { createResource, createSignal, For, Show } from "solid-js";
import { useInstance } from "../instance";
import { ErrorBanner, Modal } from "../components/ui";
import { remoteColour, useRemotes, type Remote } from "./remotes";

export function Remotes() {
  const api = useRemotes();
  const { storage } = useInstance();
  const [rows, { refetch }] = createResource(api.list);
  const [adding, setAdding] = createSignal(false);
  const [removing, setRemoving] = createSignal<Remote>();
  const [typed, setTyped] = createSignal("");
  const [error, setError] = createSignal<unknown>();
  const [busy, setBusy] = createSignal(false);
  const [colour, setColour] = createSignal("#7356b8");
  const [form, setForm] = createSignal({ name: "", url: "", instance_id: "", code: "", label: "" });
  const act = async (fn: () => Promise<unknown>) => {
    setBusy(true); setError(undefined);
    try { await fn(); await refetch(); } catch (e) { setError(e); } finally { setBusy(false); }
  };
  return <section class="nav-section" aria-label="Remotes">
    <div class="spread"><h4>Remotes</h4><button class="small ghost" aria-label="Add remote" onClick={() => { setError(undefined); setAdding(true); }}>+</button></div>
    <Show when={rows.error}><ErrorBanner error={rows.error} /></Show>
    <For each={rows()}>{(r) => <div class="spread">
      <A href={`/remote/${r.name}/`} title={`${r.url}: ${r.status}`}><span style={{ color: remoteColour(r.name, storage) }}>●</span> {r.name} · {r.status}</A>
      <button class="small ghost" aria-label={`Remove remote ${r.name}`} onClick={() => { setError(undefined); setTyped(""); setRemoving(r); }}>×</button>
    </div>}</For>
    <Show when={adding()}><Modal title="Add remote to home" onClose={() => { if (!busy()) { setAdding(false); setForm({ name: "", url: "", instance_id: "", code: "", label: "" }); } }}>
      <form class="stack" onSubmit={(e) => { e.preventDefault(); void act(async () => {
        const body = form();
        await api.add(body);
        storage?.setItem(`remote-colour:${body.name}`, colour());
        setForm({ name: "", url: "", instance_id: "", code: "", label: "" });
        setAdding(false);
      }); }}>
        <For each={["name", "url", "instance_id", "code", "label"] as const}>{(key) => <label>{key === "code" ? "Pairing code (not a link key)" : key}
          <input type={key === "code" ? "password" : "text"} required={key !== "label"} autocomplete="off" value={form()[key]} onInput={(e) => setForm({ ...form(), [key]: e.currentTarget.value })} />
        </label>}</For>
        <label>Badge colour<input type="color" value={colour()} onInput={(e) => setColour(e.currentTarget.value)} /></label>
        <Show when={error()}><ErrorBanner error={error()} /></Show>
        <button disabled={busy()}>Pair and add</button>
      </form>
    </Modal></Show>
    <Show when={removing()}>{(r) => <Modal title={`Remove remote ${r().name} from home`} onClose={() => { if (!busy()) setRemoving(undefined); }}>
      <p>This removes the home link, not the remote daemon. Type {r().name} to confirm.</p>
      <input aria-label="Remote name to remove" value={typed()} onInput={(e) => setTyped(e.currentTarget.value)} />
      <Show when={error()}><ErrorBanner error={error()} /></Show>
      <button class="danger" disabled={busy() || typed() !== r().name} onClick={() => void act(async () => { await api.remove(r().name); setRemoving(undefined); })}>Remove remote</button>
    </Modal>}</Show>
  </section>;
}
