// The config repo (PLAN §8.3, docs/api.md §25), for the operator or a
// sysadmin: the repo state, the changed files with their diffs, commit,
// sync, pull, push, the export plans of a pull, the remote, the new
// defaults that wait as `<file>.new`, and the merge banner. The logic is in
// ./model.ts. A daemon with no config repo answers `501 not_available`;
// the page then shows a short note.

import { createResource, createSignal, For, Show } from "solid-js";
import { ConfirmButton, Empty, ErrorBanner, PageHead } from "../../components/ui";
import type { ConfigChangeStatus, ConfigExportPlan, ConfigScanFinding, ConfigStatus } from "../../api/types";
import { ago } from "../../lib/format";
import { api } from "../../stores";
import {
  actions,
  aheadBehind,
  autoSyncLabel,
  changeLabel,
  diffLines,
  exportLine,
  findingLabel,
  mergeConflicts,
  NO_REPO_NOTE,
  noConfigRepo,
  remoteUrlProblem,
  SCAN_NOTE,
  scanFindings,
  shortCommit,
  waitingPlans,
  type Action,
} from "./model";

/** A unified diff as text lines. The text is file content: text nodes only. */
function DiffView(props: { text: string }) {
  const color = { add: "var(--link)", del: "var(--danger)", hunk: "var(--info)", meta: "var(--muted)", ctx: "inherit" } as const;
  return (
    <Show when={props.text} fallback={<p class="muted small">No diff.</p>}>
      <pre class="mono small" style={{ "overflow-x": "auto", margin: 0 }}>
        <For each={diffLines(props.text)}>{(l) => <div style={{ color: color[l.kind] }}>{l.text || " "}</div>}</For>
      </pre>
    </Show>
  );
}

/** One changed file. A click loads its diff. */
function ChangeRow(props: { path: string; status: ConfigChangeStatus; onError: (e: unknown) => void }) {
  const [open, setOpen] = createSignal(false);
  const [diff] = createResource(
    () => (open() ? props.path : null),
    (p) => api.config.diff(p).then((d) => d.diff).catch((e: unknown) => (props.onError(e), "")),
  );
  const c = () => changeLabel(props.status);
  return (
    <li class="stack" style={{ gap: "4px" }}>
      <div class="row">
        <button class="ghost small" onClick={() => setOpen(!open())} aria-expanded={open()}>
          {open() ? "▾" : "▸"} <span class="mono">{props.path}</span>
        </button>
        <span class={`chip ${c().tone}`}>{c().label}</span>
      </div>
      <Show when={open()}>
        <Show when={!diff.loading} fallback={<span class="muted small">Loading…</span>}>
          <DiffView text={diff() ?? ""} />
        </Show>
      </Show>
    </li>
  );
}

/** A button with the reason in its title when it is off. */
function ActionButton(props: { label: string; action: Action; onClick: () => void; primary?: boolean }) {
  return (
    <button class={props.primary ? "primary" : undefined} disabled={!props.action.enabled} title={props.action.reason ?? ""} onClick={() => props.onClick()}>
      {props.label}
    </button>
  );
}

/** The export files of the last pull or sync, with the plan summary of each. */
function ExportPlans(props: { list: ConfigExportPlan[] }) {
  return (
    <ul class="stack small" style={{ gap: "4px", margin: 0 }}>
      <For each={props.list}>
        {(x) => (
          <li>
            <span class="mono">{x.path}</span>: <span class={exportLine(x).tone ? `chip ${exportLine(x).tone}` : ""}>{exportLine(x).text}</span>
            <Show when={x.plan?.errors.length || x.plan?.warnings.length}>
              <ul class="muted" style={{ margin: "2px 0 0" }}>
                <For each={[...(x.plan?.errors ?? []), ...(x.plan?.warnings ?? [])]}>
                  {(i) => (
                    <li>
                      {i.level}: <span class="mono">{i.at}</span> {i.message}
                    </li>
                  )}
                </For>
              </ul>
            </Show>
            <Show when={x.plan?.entries.some((e) => e.action !== "unchanged")}>
              <ul class="muted" style={{ margin: "2px 0 0" }}>
                <For each={x.plan!.entries.filter((e) => e.action !== "unchanged")}>
                  {(e) => (
                    <li>
                      {e.action} {e.kind} <span class="mono">{e.key}</span>
                      <Show when={e.reason}> ({e.reason})</Show>
                    </li>
                  )}
                </For>
              </ul>
            </Show>
          </li>
        )}
      </For>
    </ul>
  );
}

/** The remote: URL, credential name, and branch. Remove deletes it. */
function RemoteEditor(props: { status: ConfigStatus; busy: boolean; onSave: (url: string, credential: string, branch: string) => void; onRemove: () => void }) {
  const [url, setUrl] = createSignal(props.status.remote?.url ?? "");
  const [credential, setCredential] = createSignal(props.status.remote?.credential ?? "");
  const [branch, setBranch] = createSignal(props.status.remote?.branch ?? "main");
  const problem = () => (url() ? remoteUrlProblem(url()) : null);
  return (
    <form
      class="stack"
      onSubmit={(e) => {
        e.preventDefault();
        if (!problem()) props.onSave(url().trim(), credential().trim(), branch().trim());
      }}
    >
      <div class="form-grid">
        <label style={{ "grid-column": "1 / -1" }}>
          URL
          <input class="mono" value={url()} onInput={(e) => setUrl(e.currentTarget.value)} placeholder="https://git.example.com/ops/terrarium-config.git" required />
          <Show when={problem()} fallback={<span class="muted small">Do not put a user or a password in the URL. Use a credential.</span>}>
            {(p) => (
              <span class="small" style={{ color: "var(--danger)" }}>
                {p()}
              </span>
            )}
          </Show>
        </label>
        <label>
          Credential name
          <input value={credential()} onInput={(e) => setCredential(e.currentTarget.value)} placeholder="config-deploy (optional)" />
        </label>
        <label>
          Branch
          <input value={branch()} onInput={(e) => setBranch(e.currentTarget.value)} placeholder="main" />
        </label>
      </div>
      <div class="row">
        <button class="primary" type="submit" disabled={props.busy || !url().trim() || !!problem()}>
          Save remote
        </button>
        <Show when={props.status.remote}>
          <ConfirmButton label="Remove" confirmLabel="Yes, remove the remote" disabled={props.busy} onConfirm={() => props.onRemove()} />
        </Show>
      </div>
    </form>
  );
}

export default function ConfigPage() {
  const [status, { refetch: refetchStatus, mutate: setStatus }] = createResource(() => api.config.status());
  const [defaults, { refetch: refetchDefaults, mutate: setDefaults }] = createResource(() => api.config.defaults().catch(() => []));
  const [log, { refetch: refetchLog }] = createResource(() => api.config.log(10).catch(() => []));
  const [error, setError] = createSignal<unknown>(null);
  const [findings, setFindings] = createSignal<ConfigScanFinding[] | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [exports, setExports] = createSignal<ConfigExportPlan[]>([]);
  const [message, setMessage] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [openDefault, setOpenDefault] = createSignal<string | null>(null);

  const refetch = () => {
    void refetchStatus();
    void refetchDefaults();
    void refetchLog();
  };
  const acts = () => (status() ? actions(status()!, { busy: busy(), message: message(), pendingPlans: waitingPlans(exports()) }) : null);

  /** Run one call. A scan block shows its findings; a conflict reads the status again for the banner. */
  const run = async <T,>(fn: () => Promise<T>, done?: (r: T) => void) => {
    setBusy(true);
    setError(null);
    setFindings(null);
    setNotice(null);
    try {
      done?.(await fn());
    } catch (e) {
      const f = scanFindings(e);
      if (f) setFindings(f);
      else setError(e);
      // A merge conflict: show the banner at once; the status read follows.
      const c = mergeConflicts(e);
      if (c && status()) setStatus({ ...status()!, merging: true, conflicts: c });
    } finally {
      setBusy(false);
      refetch();
    }
  };

  const commit = () =>
    run(
      () => api.config.commit({ message: message().trim() }),
      (r) => {
        setMessage("");
        setNotice(r.commit ? `Committed ${shortCommit(r.commit)}: ${r.files.length} file(s).` : "There was nothing to commit.");
      },
    );
  const sync = () =>
    run(
      () => api.config.sync(message().trim() ? { message: message().trim() } : {}),
      (r) => {
        setMessage("");
        setExports(r.export);
        setNotice(`Synced. ${r.committed.commit ? `Committed ${shortCommit(r.committed.commit)}. ` : ""}${r.updated ? `Pulled ${r.changed.length} changed file(s). ` : ""}${r.pushed ? "Pushed." : "Nothing to push."}`);
      },
    );
  const pull = (apply: boolean) =>
    run(
      () => api.config.pull({ apply }),
      (r) => {
        setExports(r.export);
        setNotice(apply ? "Applied the export changes." : r.updated ? `Pulled ${r.changed.length} changed file(s).` : "The repo has every commit of the remote.");
      },
    );
  const push = () => run(() => api.config.push(), (r) => setNotice(r.pushed ? "Pushed." : "Nothing to push."));
  const abort = () => run(() => api.config.abort(), () => setNotice("The merge is aborted. No local commit is lost."));
  const saveRemote = (url: string, credential: string, branch: string) =>
    run(
      () => api.config.setRemote({ url, ...(credential ? { credential } : {}), ...(branch ? { branch } : {}) }),
      (s) => {
        setStatus(s);
        setNotice("The remote is saved.");
      },
    );
  const removeRemote = () =>
    run(
      () => api.config.clearRemote(),
      (s) => {
        setStatus(s);
        setNotice("The remote is removed.");
      },
    );
  const takeDefault = (path: string, accept: boolean) =>
    run(
      () => (accept ? api.config.acceptDefault(path) : api.config.keepDefault(path)),
      (rest) => {
        setDefaults(rest);
        setNotice(accept ? `Took the new default of ${path}. Commit to keep it in the repo.` : `Kept your ${path}.`);
      },
    );

  return (
    <>
      <PageHead title="Config">
        <button onClick={() => refetch()} disabled={busy()}>
          Refresh
        </button>
      </PageHead>
      <Show when={!noConfigRepo(status.error)} fallback={<p class="notice">{NO_REPO_NOTE}</p>}>
        <ErrorBanner error={error() ?? status.error} onDismiss={() => setError(null)} />
        <Show when={notice()}>
          <p class="notice small" role="status" style={{ margin: 0 }}>
            {notice()}
          </p>
        </Show>
        <Show when={findings()}>
          {(f) => (
            <div class="error-banner" role="alert">
              <div class="stack" style={{ gap: "4px" }}>
                <strong>The secret scan blocked the commit.</strong>
                <ul class="mono small" style={{ margin: 0 }}>
                  <For each={f()}>{(x) => <li>{findingLabel(x)}</li>}</For>
                </ul>
                <span class="small">{SCAN_NOTE}</span>
              </div>
              <button class="ghost small" onClick={() => setFindings(null)} aria-label="Dismiss">
                ✕
              </button>
            </div>
          )}
        </Show>

        <Show when={status()} fallback={<Empty>{status.loading ? "Loading…" : "No status."}</Empty>}>
          {(st) => (
            <>
              <Show when={st().merging}>
                <div class="error-banner" role="alert">
                  <div class="stack" style={{ gap: "4px" }}>
                    <strong>A merge stopped on a conflict.</strong>
                    <span class="small">Edit these files, then commit. Or abort the merge. No local commit is lost.</span>
                    <ul class="mono small" style={{ margin: 0 }}>
                      <For each={st().conflicts}>{(p) => <li>{p}</li>}</For>
                    </ul>
                  </div>
                  <ActionButton label="Abort merge" action={acts()!.abort} onClick={() => void abort()} />
                </div>
              </Show>

              <section class="card-box">
                <dl class="kv">
                  <dt>Directory</dt>
                  <dd class="mono">{st().dir}</dd>
                  <dt>Branch</dt>
                  <dd>
                    <span class="mono">{st().branch}</span>
                    <Show when={!st().initialized}>
                      <span class="muted small"> (not initialized: the first commit makes the repo)</span>
                    </Show>
                  </dd>
                  <dt>Head</dt>
                  <dd class="mono">{shortCommit(st().head)}</dd>
                  <dt>Remote</dt>
                  <dd>
                    <Show when={st().remote} fallback={<span class="muted">none</span>}>
                      {(r) => (
                        <>
                          <span class="mono">{r().url}</span> <span class="muted">({r().branch})</span>
                          <Show when={r().credential}>
                            {" "}
                            <span class="chip">credential {r().credential}</span>
                          </Show>
                        </>
                      )}
                    </Show>
                  </dd>
                  <dt>Sync state</dt>
                  <dd>{aheadBehind(st())}</dd>
                  <dt>Auto-sync</dt>
                  <dd>{autoSyncLabel(st().auto_sync)}</dd>
                </dl>
              </section>

              <section class="card-box stack">
                <h3 style={{ margin: 0 }}>Changes</h3>
                <Show when={st().changes.length} fallback={<span class="muted small">No local changes.</span>}>
                  <ul class="stack" style={{ gap: "6px", margin: 0, "padding-left": 0, "list-style": "none" }}>
                    <For each={st().changes}>{(c) => <ChangeRow path={c.path} status={c.status} onError={setError} />}</For>
                  </ul>
                </Show>
                <label>
                  Commit message
                  <textarea rows={2} value={message()} onInput={(e) => setMessage(e.currentTarget.value)} placeholder="Raise the lead to Opus" />
                </label>
                <div class="row" style={{ "flex-wrap": "wrap" }}>
                  <ActionButton label="Commit" primary action={acts()!.commit} onClick={() => void commit()} />
                  <ActionButton label="Sync" action={acts()!.sync} onClick={() => void sync()} />
                  <ActionButton label="Pull" action={acts()!.pull} onClick={() => void pull(false)} />
                  <ActionButton label="Push" action={acts()!.push} onClick={() => void push()} />
                  <Show when={acts()!.showApplyExport}>
                    <ActionButton label="Apply export" action={acts()!.applyExport} onClick={() => void pull(true)} />
                  </Show>
                </div>
                <span class="muted small">Sync commits the changes (with the message, or a default one), pulls, and pushes. A commit writes the export of the store first.</span>
              </section>

              <Show when={exports().length || st().export_pending}>
                <section class="card-box stack">
                  <h3 style={{ margin: 0 }}>Export changes</h3>
                  <Show when={st().export_pending}>
                    <p class="notice small" style={{ margin: 0 }}>
                      A pull brought changes in export/. They wait. Apply export applies them as fleets.
                    </p>
                  </Show>
                  <Show when={exports().length}>
                    <ExportPlans list={exports()} />
                  </Show>
                </section>
              </Show>

              <section class="card-box stack">
                <h3 style={{ margin: 0 }}>New defaults</h3>
                <Show when={defaults()?.length} fallback={<span class="muted small">No new defaults wait.</span>}>
                  <span class="muted small">An update brought new default files. Each waits as &lt;file&gt;.new. Accept takes the new file. Keep keeps your file.</span>
                  <ul class="stack" style={{ gap: "6px", margin: 0, "padding-left": 0, "list-style": "none" }}>
                    <For each={defaults()}>
                      {(d) => (
                        <li class="stack" style={{ gap: "4px" }}>
                          <div class="row" style={{ "flex-wrap": "wrap" }}>
                            <button class="ghost small" onClick={() => setOpenDefault(openDefault() === d.path ? null : d.path)} aria-expanded={openDefault() === d.path}>
                              {openDefault() === d.path ? "▾" : "▸"} <span class="mono">{d.path}</span>
                            </button>
                            <button class="small" disabled={busy()} onClick={() => void takeDefault(d.path, true)}>
                              Accept
                            </button>
                            <button class="small" disabled={busy()} onClick={() => void takeDefault(d.path, false)}>
                              Keep mine
                            </button>
                          </div>
                          <Show when={openDefault() === d.path}>
                            <DiffView text={d.diff} />
                          </Show>
                        </li>
                      )}
                    </For>
                  </ul>
                </Show>
              </section>

              <section class="card-box stack">
                <h3 style={{ margin: 0 }}>Remote</h3>
                {/* A new remote resets the form; another status read keeps what the user typed. */}
                <Show when={JSON.stringify(st().remote ?? null)} keyed>
                  {(_remote: string) => <RemoteEditor status={st()} busy={busy()} onSave={(u, c, b) => void saveRemote(u, c, b)} onRemove={() => void removeRemote()} />}
                </Show>
              </section>

              <section class="card-box stack">
                <h3 style={{ margin: 0 }}>Recent commits</h3>
                <Show when={log()?.length} fallback={<span class="muted small">No commits.</span>}>
                  <div class="table-wrap">
                    <table>
                      <tbody>
                        <For each={log()}>
                          {(l) => (
                            <tr>
                              <td class="mono">{shortCommit(l.commit)}</td>
                              <td>{l.subject}</td>
                              <td class="muted small">{l.author}</td>
                              <td class="muted small" title={l.at}>
                                {ago(l.at)}
                              </td>
                            </tr>
                          )}
                        </For>
                      </tbody>
                    </table>
                  </div>
                </Show>
              </section>
            </>
          )}
        </Show>
      </Show>
    </>
  );
}
