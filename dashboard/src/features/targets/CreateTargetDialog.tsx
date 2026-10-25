// Define a target (docs/api.md §17.2): `POST /api/targets`. The scope is
// the selected project by default; "global" is an explicit choice. The
// daemon answers `403 forbidden` when the viewer may not define a target in
// that scope (§10): a global target needs a sysadmin.

import { For, Show } from "solid-js";
import { ErrorBanner, Modal } from "../../components/ui";
import type { Target, TargetKind } from "../../api/types";
import { api, projects } from "../../stores";
import { createCreateTargetDialog, GLOBAL, showsField, type CreateOutcome } from "./create";

const KINDS: readonly TargetKind[] = ["docker", "vmlab", "host", "ssh"];

export function CreateTargetDialog(props: { project: string | null; onClose: () => void; onCreated: (t: Target) => void }) {
  const dlg = createCreateTargetDialog(api, props.project);
  const f = dlg.form;
  const shows = (k: Parameters<typeof showsField>[1]) => showsField(f().kind, k);
  const forbidden = () => {
    const o = dlg.outcome();
    return o.kind === "forbidden" ? (o as Extract<CreateOutcome, { kind: "forbidden" }>) : null;
  };
  const submit = async (e: Event) => {
    e.preventDefault();
    const t = await dlg.submit();
    if (t) props.onCreated(t);
  };
  const text = (key: "host" | "address" | "port" | "credential" | "image" | "container" | "lab" | "machine", label: string, placeholder = "") => (
    <Show when={shows(key)}>
      <label>
        {label}
        <input value={f()[key]} placeholder={placeholder} onInput={(e) => dlg.set(key, e.currentTarget.value)} />
      </label>
    </Show>
  );
  return (
    <Modal title="New target" onClose={() => props.onClose()}>
      <form class="stack" onSubmit={submit} aria-label="New target">
        <div class="form-grid">
          <label>
            Name
            <input value={f().name} required placeholder="pg-test" onInput={(e) => dlg.set("name", e.currentTarget.value)} />
          </label>
          <label>
            Scope
            <select aria-label="Scope" value={f().scope} onChange={(e) => dlg.set("scope", e.currentTarget.value)} required>
              <option value="" disabled selected={!f().scope}>
                Pick a project, or global
              </option>
              <For each={projects.projects()}>
                {(p) => (
                  <option value={p.name} selected={p.name === f().scope}>
                    Project {p.name}
                  </option>
                )}
              </For>
              <option value={GLOBAL} selected={f().scope === GLOBAL}>
                Global (all projects; needs sysadmin)
              </option>
            </select>
          </label>
          <label>
            Kind
            <select aria-label="Kind" value={f().kind} onChange={(e) => dlg.set("kind", e.currentTarget.value as TargetKind)}>
              <For each={KINDS}>
                {(k) => (
                  <option value={k} selected={k === f().kind}>
                    {k}
                  </option>
                )}
              </For>
            </select>
          </label>
          {text("host", "Host agent", "lab1")}
          {text("address", "Address", "build@buildbox.lan")}
          {text("port", "Port", "22")}
          {text("credential", "Credential (an ssh_key name)", "buildbox-key")}
          {text("image", "Image", "alpine:3")}
          {text("container", "Container (an existing one)", "")}
          {text("lab", "Lab (an absolute path)", "/labs/win11/vmlab.wcl")}
          {text("machine", "Machine", "win11")}
        </div>
        <Show when={shows("ephemeral")}>
          <div class="row" style={{ "flex-wrap": "wrap", gap: "12px" }}>
            <label class="row small">
              <input type="checkbox" checked={f().ephemeral} onChange={(e) => dlg.set("ephemeral", e.currentTarget.checked)} />
              ephemeral
            </label>
            <label class="row small" title="An ephemeral target with keep stays after an agent stops">
              <input type="checkbox" checked={f().keep} disabled={!f().ephemeral} onChange={(e) => dlg.set("keep", e.currentTarget.checked)} />
              keep
            </label>
            <Show when={shows("snapshot_before_task")}>
              <label class="row small">
                <input type="checkbox" checked={f().snapshot_before_task} onChange={(e) => dlg.set("snapshot_before_task", e.currentTarget.checked)} />
                snapshot before each task
              </label>
            </Show>
          </div>
        </Show>
        <p class="small muted">
          {f().scope === GLOBAL
            ? "A global target: any project may use it. Only a sysadmin may define it."
            : f().scope
              ? `A target of project ${f().scope}: only its agents may use it. A project_admin of ${f().scope} may define it.`
              : "Choose a scope."}
        </p>
        <Show when={forbidden()}>
          {(o) => (
            <div class="error-banner" role="alert">
              <div class="stack" style={{ gap: "2px" }}>
                <strong>Refused: you may not define a {o().scope === GLOBAL ? "global target" : `target of project ${o().scope}`}.</strong>
                <span class="small">{o().message}</span>
                <span class="small muted">{o().scope === GLOBAL ? "A global target needs the sysadmin role. Choose a project." : `It needs project_admin of ${o().scope}.`}</span>
              </div>
            </div>
          )}
        </Show>
        <Show when={dlg.outcome().kind === "invalid" ? (dlg.outcome() as { message: string }).message : null}>{(m) => <p class="error-banner small" role="alert">{m()}</p>}</Show>
        <ErrorBanner error={dlg.outcome().kind === "error" ? (dlg.outcome() as { error: unknown }).error : null} />
        <div class="row" style={{ "justify-content": "flex-end" }}>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
          <button class="primary" type="submit" disabled={dlg.outcome().kind === "sending"}>
            {dlg.outcome().kind === "sending" ? "Creating…" : "Create"}
          </button>
        </div>
      </form>
    </Modal>
  );
}
