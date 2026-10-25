// Define a target (docs/api.md §17.2): `POST /api/targets`. The scope is
// the selected project by default; "global" is an explicit choice. The
// daemon answers `403 forbidden` when the viewer may not define a target in
// that scope (§10): a global target needs a sysadmin. A project target
// gets the name `<project>-<name>`, and a project holds at most
// TERRARIUM_MAX_TARGETS_PER_PROJECT targets (`409`). A vmlab target takes a
// lab dir, a lab repo of the project, or the lab in each holder's workspace.

import { createResource, For, Show } from "solid-js";
import { ErrorBanner, Modal } from "../../components/ui";
import type { Target, TargetKind } from "../../api/types";
import { api, projects } from "../../stores";
import { capSentence } from "../../lib/target-runtime";
import { createCreateTargetDialog, GLOBAL, LAB_SOURCES, showsField, type CreateOutcome } from "./create";

const KINDS: readonly TargetKind[] = ["docker", "vmlab", "host", "ssh"];

export function CreateTargetDialog(props: { project: string | null; onClose: () => void; onCreated: (t: Target) => void }) {
  const dlg = createCreateTargetDialog(api, props.project);
  const f = dlg.form;
  const shows = (k: Parameters<typeof showsField>[1]) => showsField(f().kind, k, f().labSource);
  const projectScope = () => (f().scope && f().scope !== GLOBAL ? f().scope : null);
  // The lab repo picker: the repos of the project of the scope.
  const [repos] = createResource(
    () => (f().kind === "vmlab" && f().labSource === "repo" ? projectScope() : null),
    (p) => api.projects.repos(p).catch(() => []),
  );
  const source = () => LAB_SOURCES.find((x) => x.id === f().labSource)!;
  const cap = () => {
    const o = dlg.outcome();
    return o.kind === "cap" ? (o as Extract<CreateOutcome, { kind: "cap" }>).cap : null;
  };
  const forbidden = () => {
    const o = dlg.outcome();
    return o.kind === "forbidden" ? (o as Extract<CreateOutcome, { kind: "forbidden" }>) : null;
  };
  const submit = async (e: Event) => {
    e.preventDefault();
    const t = await dlg.submit();
    if (t) props.onCreated(t);
  };
  const text = (key: "host" | "address" | "port" | "credential" | "image" | "container" | "lab" | "machine" | "lab_ref" | "lab_path", label: string, placeholder = "") => (
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
          <Show when={shows("labSource")}>
            <label>
              Lab source
              <select aria-label="Lab source" value={f().labSource} onChange={(e) => dlg.set("labSource", e.currentTarget.value as typeof LAB_SOURCES[number]["id"])}>
                <For each={LAB_SOURCES}>
                  {(x) => (
                    <option value={x.id} selected={x.id === f().labSource}>
                      {x.label}
                    </option>
                  )}
                </For>
              </select>
            </label>
          </Show>
          {text("lab", "Lab directory (an absolute path on the host)", "/labs/win11")}
          <Show when={shows("lab_repo")}>
            <label>
              Lab repo
              <select aria-label="Lab repo" value={f().lab_repo} onChange={(e) => dlg.set("lab_repo", e.currentTarget.value)} disabled={!projectScope()}>
                <option value="" disabled selected={!f().lab_repo}>
                  {!projectScope() ? "Needs a project scope" : repos.loading ? "Loading…" : repos()?.length ? "Pick a repo of the project" : "The project has no repos"}
                </option>
                <For each={repos() ?? []}>
                  {(r) => (
                    <option value={r.name} selected={r.name === f().lab_repo}>
                      {r.name}
                      {r.default_branch ? ` (default ${r.default_branch})` : ""}
                    </option>
                  )}
                </For>
              </select>
            </label>
          </Show>
          {text("lab_ref", "Ref (a branch, a tag, or a commit)", repos()?.find((r) => r.name === f().lab_repo)?.default_branch ?? "default branch")}
          {text("lab_path", "Lab path (the dir that holds vmlab.wcl)", "the root")}
          {text("machine", f().kind === "vmlab" && f().labSource !== "dir" ? "Machine (gets the workspace)" : "Machine", "win11")}
        </div>
        <Show when={projectScope() && f().name.trim()}>
          <p class="small muted" aria-label="Full name" style={{ margin: 0 }}>
            The target name is <strong class="mono">{dlg.fullName()}</strong>
            {dlg.fullName() !== f().name.trim() ? ` (a project target gets the prefix ${projectScope()}-)` : ""}.
          </p>
        </Show>
        <Show when={shows("labSource")}>
          <p class="small muted" aria-label="Lab source help" style={{ margin: 0 }}>
            {source().help}
            {f().labSource !== "dir" && f().scope === GLOBAL ? " It needs a project scope." : ""}
          </p>
        </Show>
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
        <Show when={cap()}>
          {(c) => (
            <div class="error-banner" role="alert" aria-label="Project cap">
              <div class="stack" style={{ gap: "2px" }}>
                <strong>Refused: {capSentence(c())}</strong>
                <span class="small">{c().message}</span>
                <span class="small muted">The cap is TERRARIUM_MAX_TARGETS_PER_PROJECT on the daemon.</span>
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
