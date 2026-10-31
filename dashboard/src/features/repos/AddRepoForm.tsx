// The Add repo form (`POST /api/projects/{name}/repos`, project_admin, docs/api.md §5).
// The Repos page shows it in a card; the "+" of the Repos node of the sidebar
// tree shows it in a dialog (AddRepoDialog). Both put the new repo into the
// repos store, so the tree shows it at once.

import { createSignal } from "solid-js";
import type { NewRepo, Repo } from "../../api/types";
import { ErrorBanner, Modal } from "../../components/ui";
import { api, repos } from "../../stores";

/** The request body of a form: the URL, and only the other fields that have text. */
export function newRepoBody(f: NewRepo): NewRepo {
  const body: NewRepo = { url: f.url.trim() };
  for (const k of ["name", "default_branch", "credential", "path"] as const) if (f[k]?.trim()) body[k] = f[k]!.trim();
  return body;
}

/** Attach a repo. Attaching does not clone. `bare` drops the card and the heading (for a dialog). */
export function AddRepoForm(props: { project: string; onAdded: (repo: Repo) => void; bare?: boolean }) {
  const [form, setForm] = createSignal<NewRepo>({ url: "" });
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const set = (k: keyof NewRepo, v: string) => setForm({ ...form(), [k]: v });
  return (
    <form
      class={props.bare ? "stack" : "card-box stack"}
      style={{ gap: "8px", "margin-bottom": props.bare ? undefined : "16px" }}
      aria-label="Add a repo"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        try {
          const repo = await api.projects.attachRepo(props.project, newRepoBody(form()));
          repos.added(repo);
          setForm({ url: "" });
          setError(null);
          props.onAdded(repo);
        } catch (err) {
          setError(err);
        } finally {
          setBusy(false);
        }
      }}
    >
      {props.bare ? null : <h2 style={{ margin: 0 }}>Add a repo</h2>}
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <div class="form-grid">
        <label>
          URL
          <input value={form().url} onInput={(e) => set("url", e.currentTarget.value)} required placeholder="https://github.com/me/infra.git" />
        </label>
        <label>
          Name (empty: from the URL)
          <input value={form().name ?? ""} onInput={(e) => set("name", e.currentTarget.value)} />
        </label>
        <label>
          Default branch
          <input value={form().default_branch ?? ""} onInput={(e) => set("default_branch", e.currentTarget.value)} placeholder="main" />
        </label>
        <label>
          Credential (a key name, never the secret)
          <input value={form().credential ?? ""} onInput={(e) => set("credential", e.currentTarget.value)} placeholder="github-deploy" />
        </label>
        <label>
          Checkout path
          <input value={form().path ?? ""} onInput={(e) => set("path", e.currentTarget.value)} placeholder="infra" />
        </label>
      </div>
      <div>
        <button class="primary" type="submit" disabled={busy()}>
          Add
        </button>
      </div>
    </form>
  );
}

/** The Add repo form in a dialog, for the "+" of the Repos node of the sidebar tree. */
export function AddRepoDialog(props: { project: string; onClose: () => void; onAdded: (repo: Repo) => void }) {
  return (
    <Modal title={`Add a repo to ${props.project}`} onClose={() => props.onClose()}>
      <AddRepoForm project={props.project} bare onAdded={(r) => props.onAdded(r)} />
    </Modal>
  );
}
