// The "Harness versions" table: the active version and the version dirs on
// disk, the tested range, the latest release on npm, a status chip, and the
// live agents that still run an older version (GET /api/harnesses?latest=true).
// A row with a newer release has an "Update" button, and "Update all" updates
// every harness. The daemon runs npm in the container into a new version dir
// and switches to it (POST /api/harnesses/{name}/update); no host agent and no
// image rebuild. Running agents keep their version until their next restart,
// so nothing refuses the update. A copyable CLI line under the table is the
// fallback.
//
// The "Updates" column shows the update policy of each harness (docs/api.md
// §4, "Auto-update and pins"): `auto` (the background check installs each
// newer release) or `pinned <version>`, and the last check. "Roll back and
// pin" switches to the previous version and pins it; "Pin current" pins the
// active version; "Unpin" starts the auto-update again; "Check now" runs the
// check at once (POST /api/harnesses/check).

import { createResource, createSignal, For, Show } from "solid-js";
import { ConfirmButton, CopyButton, ErrorBanner } from "../../components/ui";
import { api } from "../../stores";
import type { HarnessUpdateResult } from "../../api/types";
import {
  canPinCurrent,
  canUnpin,
  checkLine,
  harnessStatus,
  lastCheckLine,
  offPin,
  oldAgentLine,
  oldAgents,
  otherVersions,
  outOfRange,
  pinLine,
  policyChip,
  policyNote,
  publishedDay,
  rangeWarning,
  resultLine,
  resultNotes,
  rollbackTarget,
  scheduleNote,
  updateCommand,
  updateFailure,
  withUpdate,
  type UpdateFailure,
} from "./harnesses";

/**
 * One call, as the box under the table shows it. `name` is a harness, "all", or "the check". An update fills
 * `results`; a pin, an unpin, or a check fills `lines`.
 */
interface UpdateRun {
  name: string;
  /** "update", "pin", "unpin", or "check": the words of the status line. */
  verb: string;
  done: boolean;
  results: HarnessUpdateResult[];
  lines: string[];
  failure: UpdateFailure | null;
}

export function HarnessVersions() {
  const [report, { refetch }] = createResource((_: unknown, info: { refetching?: unknown }) => api.harnesses(info.refetching === true, true));
  const [run, setRun] = createSignal<UpdateRun | null>(null);
  const busy = () => !!run() && !run()!.done;

  const update = async (name: string) => {
    setRun({ name, verb: "update", done: false, results: [], lines: [], failure: null });
    try {
      const results = name === "all" ? (await api.harnessUpdateAll()).results : [await api.harnessUpdate(name, { version: "latest" })];
      setRun({ name, verb: "update", done: true, results, lines: [], failure: null });
    } catch (e) {
      setRun({ name, verb: "update", done: true, results: [], lines: [], failure: updateFailure(e) });
    }
    void refetch(true);
  };

  /** A pin (to `version`, else the previous version), an unpin, or a check; the box shows its lines. */
  const act = async (verb: "pin" | "unpin" | "check", name: string, version?: string) => {
    setRun({ name, verb, done: false, results: [], lines: [], failure: null });
    try {
      let lines: string[];
      if (verb === "check") lines = (await api.harnessCheck()).results.map(checkLine);
      else {
        const r = verb === "pin" ? await api.harnessPin(name, version) : await api.harnessUnpin(name);
        lines = [pinLine(r), ...(r.note ? [r.note] : []), ...(r.removed?.length ? [`${r.harness}: removed the unused version${r.removed.length > 1 ? "s" : ""} ${r.removed.join(", ")}`] : [])];
      }
      setRun({ name, verb, done: true, results: [], lines, failure: null });
    } catch (e) {
      setRun({ name, verb, done: true, results: [], lines: [], failure: updateFailure(e) });
    }
    void refetch(true);
  };

  return (
    <section class="card-box" aria-label="Harness versions" style={{ "margin-bottom": "14px" }}>
      <div class="spread">
        <h3 style={{ margin: 0 }}>
          Harness versions
          <Show when={outOfRange(report.latest).length}>
            {(n) => <span class="chip bad" style={{ "margin-left": "8px" }}>{n()} unsupported</span>}
          </Show>
          <Show when={withUpdate(report.latest).length}>
            {(n) => <span class="chip warn" style={{ "margin-left": "8px" }}>{n()} update{n() > 1 ? "s" : ""} available</span>}
          </Show>
        </h3>
        <div class="row" style={{ gap: "8px" }}>
          <button class="small" disabled={busy()} title="Ask npm now, and install each newer release of a harness that is not pinned" onClick={() => void act("check", "the check")}>
            Check now
          </button>
          <button class="small" onClick={() => void refetch(true)} disabled={report.loading}>
            {report.loading ? "Checking…" : "Refresh"}
          </button>
        </div>
      </div>
      <ErrorBanner error={report.error} />
      <Show when={report.latest}>
        {(r) => (
          <>
            <div class="table-wrap">
              <table aria-label="Harness versions">
                <thead>
                  <tr>
                    <th>Harness</th>
                    <th>Installed</th>
                    <th>Tested range</th>
                    <th>Latest</th>
                    <th>Status</th>
                    <th>Updates</th>
                    <th>Agents</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  <For each={r().harnesses}>
                    {(h) => {
                      const s = harnessStatus(h);
                      const p = policyChip(h);
                      return (
                        <tr>
                          <td>
                            <strong>{h.harness}</strong>
                            <div class="small muted mono">{h.package}</div>
                          </td>
                          <td class="mono" title={offPin(h) ? `Not the pinned version (${h.pinned})` : `The pinned version`}>
                            {h.installed ?? "—"}
                            <Show when={offPin(h)}>
                              <div class="small muted">pin {h.pinned}</div>
                            </Show>
                            <Show when={otherVersions(h).length}>
                              <div class="small muted" title="Version dirs on disk: kept while an agent runs them">
                                also on disk: {otherVersions(h).join(", ")}
                              </div>
                            </Show>
                          </td>
                          <td class="mono">{h.tested}</td>
                          <td class="mono" title={h.latest_error}>
                            <Show when={h.latest} fallback={<span class="muted">{h.latest_error ? "npm not reachable" : "—"}</span>}>
                              {(l) => (
                                <>
                                  {l().version}
                                  <Show when={publishedDay(l().published_at)}>{(d) => <div class="small muted">published {d()}</div>}</Show>
                                </>
                              )}
                            </Show>
                          </td>
                          <td>
                            <span class={`chip ${s.cls}`} title={s.title} data-status={s.status}>
                              {s.status}
                            </span>
                            <Show when={rangeWarning(h)}>
                              {(w) => (
                                <div class="small" role="note" style={{ color: "var(--danger)", "max-width": "28ch", "margin-top": "4px" }}>
                                  {w()}
                                </div>
                              )}
                            </Show>
                          </td>
                          <td class="small" aria-label={`Updates of ${h.harness}`}>
                            <span class={`chip ${p.cls}`} title={p.title} data-policy={h.auto_update?.policy}>
                              {p.label}
                            </span>
                            <Show when={lastCheckLine(h)}>{(l) => <div class="muted" title="The last auto-update check">{l()}</div>}</Show>
                            <Show when={h.auto_update?.changelog}>
                              {(u) => (
                                <div>
                                  <a href={u()} target="_blank" rel="noopener noreferrer">
                                    changelog
                                  </a>
                                </div>
                              )}
                            </Show>
                          </td>
                          <td class="small" aria-label={`Agents on ${h.harness}`}>
                            <Show when={(h.agents ?? []).length} fallback={<span class="muted">—</span>}>
                              <div class="muted">{(h.agents ?? []).length} running</div>
                            </Show>
                            <For each={oldAgents(h)}>{(a) => <div class="chip warn mono" title="This agent keeps its version until its next restart">{oldAgentLine(a, h.active!)}</div>}</For>
                          </td>
                          <td>
                            <div class="row" style={{ gap: "6px", "flex-direction": "column", "align-items": "stretch", "white-space": "nowrap" }}>
                              <Show when={h.update === "minor" || h.update === "major"}>
                                <button class="small" disabled={busy()} title={`Update ${h.harness} to ${h.latest?.version ?? "the latest"} in the container`} onClick={() => void update(h.harness)}>
                                  Update
                                </button>
                              </Show>
                              <Show when={rollbackTarget(h)}>
                                {(v) => (
                                  <ConfirmButton
                                    class="small"
                                    label="Roll back and pin"
                                    confirmLabel={`Yes, use ${v()}`}
                                    disabled={busy()}
                                    onConfirm={() => act("pin", h.harness)}
                                  />
                                )}
                              </Show>
                              <Show when={canPinCurrent(h)}>
                                <button class="small" disabled={busy()} title={`Keep ${h.harness} at ${h.active ?? h.installed}: the auto-update does not change it`} onClick={() => void act("pin", h.harness, h.active ?? h.installed ?? undefined)}>
                                  Pin current
                                </button>
                              </Show>
                              <Show when={canUnpin(h)}>
                                <button class="small" disabled={busy()} title={`Start the auto-update of ${h.harness} again`} onClick={() => void act("unpin", h.harness)}>
                                  Unpin
                                </button>
                              </Show>
                            </div>
                          </td>
                        </tr>
                      );
                    }}
                  </For>
                </tbody>
              </table>
            </div>
            <Show when={withUpdate(r()).length > 1}>
              <div class="row" style={{ gap: "8px", "margin-top": "8px" }}>
                <button class="small" disabled={busy()} onClick={() => void update("all")} title="Update every harness and the Copilot extension to the latest on npm">
                  Update all
                </button>
              </div>
            </Show>
            <Show when={run()}>
              {(x) => (
                <div class="card-box" aria-label="Update output" role="status" style={{ margin: "10px 0 0" }}>
                  <div class="small">{runStatus(x())}</div>
                  <For each={x().lines}>{(l) => <div class="small mono">{l}</div>}</For>
                  <For each={x().results}>
                    {(res) => (
                      <>
                        <div class="small mono">
                          {resultLine(res)}
                          {res.error ? ` (${res.error})` : ""}
                        </div>
                        <For each={resultNotes(res)}>{(n) => <div class="small muted">{n}</div>}</For>
                      </>
                    )}
                  </For>
                  <Show when={x().failure}>{(f) => <ErrorBanner error={new Error(f().message)} />}</Show>
                  <Show when={x().results.map((res) => res.output).join("\n").trim() || x().failure?.output}>
                    {(out) => <pre class="mono small" style={{ "max-height": "260px", overflow: "auto" }}>{out()}</pre>}
                  </Show>
                </div>
              )}
            </Show>
            <Show when={withUpdate(r()).length > 0}>
              <div class="card-box" aria-label="Update by hand" style={{ margin: "10px 0 0" }}>
                <div class="small">Or in a shell in the container (just docker::shell):</div>
                <For each={withUpdate(r())}>
                  {(h) => (
                    <Show when={updateCommand(h)}>
                      {(cmd) => (
                        <div class="row" style={{ gap: "8px", "align-items": "center", margin: "6px 0", "flex-wrap": "wrap" }}>
                          <code class="mono">{cmd()}</code>
                          <CopyButton text={cmd()} label="Copy command" />
                        </div>
                      )}
                    </Show>
                  )}
                </For>
              </div>
            </Show>
            <p class="muted small" style={{ margin: "6px 0 0" }}>
              {scheduleNote(r())} {policyNote(r())} harnesses.toml gives the first install; see docs/getting-started.md, "Updating a harness".
            </p>
          </>
        )}
      </Show>
    </section>
  );
}

/** The status line of the box: what runs, or how it ended. */
function runStatus(x: UpdateRun): string {
  const what = x.verb === "check" ? "The check" : `The ${x.verb} of ${x.name}`;
  if (!x.done) return x.verb === "check" ? "Checking npm…" : x.verb === "update" ? `Updating ${x.name}…` : `${x.verb === "pin" ? "Pinning" : "Unpinning"} ${x.name}…`;
  return x.failure ? `${what} failed.` : `${what} finished.`;
}
