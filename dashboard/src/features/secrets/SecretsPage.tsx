// Secrets (PLAN §16b, docs/api.md §5.2). Under /secrets: every secret that
// the caller manages, with a scope filter. Under
// /p/:project/settings/secrets (a tab of the project settings screen): the
// project's secrets, then the global ones that the project can use
// (marked; a project secret hides a global one of the same name). The
// plain fields, the public key of an `ssh_key` with the authorized_keys
// line, New secret (a form for each kind), Generate SSH key, Replace
// value, Delete (a `409` names the targets that use it; "Delete anyway"
// sends `?force=true`), Reveal (the operator password, 30 seconds), and
// the grants. No value goes into a store, the URL, or storage.

import { useParams } from "@solidjs/router";
import { createResource, createSignal, For, Show } from "solid-js";
import type { Secret } from "../../api/types";
import { ConfirmButton, Empty, ErrorBanner, Modal, PageHead } from "../../components/ui";
import { ago } from "../../lib/format";
import { api, projects } from "../../stores";
import { deleteConflict, filterSecrets, fingerprint, GLOBAL, KINDS, plainFields, projectSecrets, type ListedSecret } from "./model";
import { GenerateKeyForm } from "./GenerateKey";
import { GrantsPanel } from "./GrantsPanel";
import { PublicKey } from "./PublicKey";
import { RevealPanel } from "./RevealPanel";
import { SecretDialog } from "./SecretDialog";

const COLS = 6;
const key = (s: Pick<Secret, "scope" | "name">) => `${s.scope}/${s.name}`;

export default function SecretsPage() {
  const route = useParams<{ project?: string }>();
  // "all", "global", or a project name. The project of the route wins.
  const [scope, setScope] = createSignal("all");
  const [error, setError] = createSignal<unknown>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [dialog, setDialog] = createSignal<{ secret?: Secret } | null>(null);
  const [generating, setGenerating] = createSignal(false);
  // The open panel of one secret: reveal, grants, or the targets of a refused delete.
  const [panel, setPanel] = createSignal<{ key: string; kind: "reveal" | "grants" } | null>(null);
  const [conflict, setConflict] = createSignal<{ key: string; targets: string[] } | null>(null);

  const [list, { refetch }] = createResource(
    () => ({ project: route.project ?? null }),
    async (v): Promise<Secret[]> => {
      if (!v.project) return api.secrets.list();
      // A project_admin may not read the global scope: then the project's own only.
      const [own, global] = await Promise.all([api.secrets.list({ scope: v.project }), api.secrets.list({ scope: GLOBAL }).catch(() => [] as Secret[])]);
      return [...own, ...global];
    },
  );
  const rows = (): ListedSecret[] => {
    const all = list.latest ?? [];
    if (route.project) return projectSecrets(all, route.project);
    return filterSecrets(all, scope()).map((secret) => ({ secret, global: false, hidden: false }));
  };
  /** The scopes of a new secret: the project of the route first. */
  const scopes = () => (route.project ? [route.project, GLOBAL] : [GLOBAL, ...projects.projects().map((p) => p.name)]);
  const newScope = () => route.project ?? (scope() === "all" ? GLOBAL : scope());
  const toggle = (s: Secret, kind: "reveal" | "grants") => {
    const cur = panel();
    setPanel(cur?.key === key(s) && cur.kind === kind ? null : { key: key(s), kind });
  };

  const remove = async (s: Secret, force = false) => {
    try {
      await api.secrets.remove(s.scope, s.name, force ? { force: true } : {});
      setError(null);
      setConflict(null);
      setNotice(`Deleted ${key(s)}.${force ? " The targets keep the name with no secret; an exec on them fails until a secret of that name is there again." : ""}`);
      void refetch();
    } catch (e) {
      const targets = deleteConflict(e);
      if (targets) {
        setConflict({ key: key(s), targets });
        setError(null);
      } else setError(e);
    }
  };

  return (
    <>
      <PageHead title="Secrets" level={route.project ? 2 : 1}>
        <Show when={!route.project}>
          <select value={scope()} onChange={(e) => setScope(e.currentTarget.value)} aria-label="Scope filter">
            <option value="all">All scopes</option>
            <option value={GLOBAL}>Global</option>
            <For each={projects.projects()}>{(p) => <option value={p.name}>Project: {p.name}</option>}</For>
          </select>
        </Show>
        <button onClick={() => void refetch()}>Refresh</button>
        <button onClick={() => setGenerating(true)}>Generate SSH key</button>
        <button class="primary" onClick={() => setDialog({})}>
          New secret
        </button>
      </PageHead>
      <p class="muted small">
        {route.project
          ? `The secrets of ${route.project}, then the global secrets that its agents and targets can use. A project secret hides a global secret of the same name for ${route.project}.`
          : "Global secrets need a sysadmin or the operator; the secrets of a project, its project_admin too."}{" "}
        An agent gets a secret only through a grant. No list shows a value; Reveal asks for the operator password.
      </p>
      <ErrorBanner error={error() ?? list.error} onDismiss={() => setError(null)} />
      <Show when={notice()}>
        <p class="notice small spread" role="status">
          {notice()}
          <button class="ghost small" onClick={() => setNotice(null)} aria-label="Dismiss">
            ✕
          </button>
        </p>
      </Show>
      <Show when={rows().length} fallback={<Empty>{list.loading ? "Loading…" : "No secrets match."}</Empty>}>
        <section class="card-box">
          <div class="table-wrap">
            <table aria-label="Secrets">
              <thead>
                <tr>
                  <th>Name</th>
                  <th>Scope</th>
                  <th>Kind</th>
                  <th>Plain fields</th>
                  <th>Updated</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                <For each={rows()}>
                  {(r) => (
                    <>
                      <tr>
                        <td>
                          <strong>{r.secret.name}</strong>
                          <div class="small muted">secret fields: {r.secret.secret_fields.join(", ") || "none"}</div>
                        </td>
                        <td>
                          <span class={`chip ${r.secret.scope === GLOBAL ? "info" : ""}`}>{r.secret.scope}</span>
                          <Show when={r.global}>
                            {" "}
                            <span class="chip" title={`A global secret that ${route.project} can use`}>
                              usable here
                            </span>
                          </Show>
                          <Show when={r.hidden}>
                            {" "}
                            <span class="chip warn" title={`The secret ${route.project}/${r.secret.name} wins for ${route.project}`}>
                              hidden by project secret
                            </span>
                          </Show>
                        </td>
                        <td title={KINDS[r.secret.kind]?.help}>
                          <span class="mono small">{r.secret.kind}</span>
                        </td>
                        <td class="small">
                          <For each={plainFields(r.secret)} fallback={<Show when={!fingerprint(r.secret)}><span class="muted">—</span></Show>}>
                            {([k, v]) => (
                              <div>
                                <span class="muted">{k}</span> <span class="mono">{v}</span>
                              </div>
                            )}
                          </For>
                          <Show when={fingerprint(r.secret)}>
                            {(fp) => (
                              <div>
                                <span class="muted">fingerprint</span> <span class="mono">{fp()}</span>
                              </div>
                            )}
                          </Show>
                        </td>
                        <td class="small" title={`created ${r.secret.created_at} by ${r.secret.created_by}; updated ${r.secret.updated_at}`}>
                          {ago(r.secret.updated_at, Date.now())}
                        </td>
                        <td>
                          <span class="row" style={{ gap: "4px", "flex-wrap": "wrap" }}>
                            <button class="small" aria-expanded={panel()?.key === key(r.secret) && panel()?.kind === "reveal"} onClick={() => toggle(r.secret, "reveal")} aria-label={`Reveal ${key(r.secret)}`}>
                              Reveal
                            </button>
                            <button class="small" aria-expanded={panel()?.key === key(r.secret) && panel()?.kind === "grants"} onClick={() => toggle(r.secret, "grants")} aria-label={`Grants of ${key(r.secret)}`}>
                              Grants
                            </button>
                            <button class="small" onClick={() => setDialog({ secret: r.secret })} aria-label={`Replace value of ${key(r.secret)}`}>
                              Replace value
                            </button>
                            <ConfirmButton class="small danger" label="Delete" onConfirm={() => remove(r.secret)} />
                          </span>
                        </td>
                      </tr>
                      <Show when={hasPublicKey(r.secret)}>
                        <tr class="key-row">
                          <td colSpan={COLS}>
                            <PublicKey secret={r.secret} />
                          </td>
                        </tr>
                      </Show>
                      <Show when={conflict()?.key === key(r.secret) ? conflict() : null}>
                        {(c) => (
                          <tr>
                            <td colSpan={COLS}>
                              <div class="error-banner spread" role="alert" aria-label={`Delete refused for ${key(r.secret)}`}>
                                <div class="stack" style={{ gap: "2px" }}>
                                  <strong>
                                    {key(r.secret)} is the login of target{c().targets.length === 1 ? "" : "s"} {c().targets.join(", ")}.
                                  </strong>
                                  <span class="small">Change the target first. Or delete anyway: each target keeps the name with no secret, and an exec on it fails until a secret of that name is there again.</span>
                                </div>
                                <span class="row" style={{ gap: "4px" }}>
                                  <button class="danger small" onClick={() => void remove(r.secret, true)}>
                                    Delete anyway
                                  </button>
                                  <button class="small" onClick={() => setConflict(null)}>
                                    Keep
                                  </button>
                                </span>
                              </div>
                            </td>
                          </tr>
                        )}
                      </Show>
                      <Show when={panel()?.key === key(r.secret) ? panel() : null}>
                        {(p) => (
                          <tr class="secret-panel-row">
                            <td colSpan={COLS}>
                              <Show when={p().kind === "reveal"} fallback={<GrantsPanel secret={r.secret} onClose={() => setPanel(null)} />}>
                                <RevealPanel secret={r.secret} onClose={() => setPanel(null)} />
                              </Show>
                            </td>
                          </tr>
                        )}
                      </Show>
                    </>
                  )}
                </For>
              </tbody>
            </table>
          </div>
        </section>
      </Show>
      <Show when={dialog()}>
        {(d) => (
          <SecretDialog
            secret={d().secret}
            scope={newScope()}
            scopes={scopes()}
            onClose={() => setDialog(null)}
            onSaved={(s) => {
              setDialog(null);
              setNotice(`${d().secret ? "Replaced the value of" : "Created"} ${key(s)}.`);
              void refetch();
            }}
          />
        )}
      </Show>
      <Show when={generating()}>
        <Modal title="Generate SSH key" onClose={() => setGenerating(false)}>
          <GenerateKeyForm
            scope={newScope()}
            scopes={scopes()}
            showResult
            onCancel={() => setGenerating(false)}
            onGenerated={(s) => {
              setNotice(`Generated ${key(s)} (${String(s.fields.fingerprint ?? "")}). Add its public key to ~/.ssh/authorized_keys on the target.`);
              void refetch();
            }}
          />
        </Modal>
      </Show>
    </>
  );
}

const hasPublicKey = (s: Secret) => s.kind === "ssh_key" && typeof s.fields.public_key === "string";
