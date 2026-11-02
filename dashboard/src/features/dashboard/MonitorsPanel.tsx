// The Monitors tab of the dashboard (docs/api.md §30): each monitor of the
// project with its state, last run, next run, an on/off switch, Run now,
// edit, and delete. The edit dialog has a script editor, the target, the
// schedule, the kind and the thresholds, and a Test run that runs the
// script once and shows the raw output and the parsed result before a save.

import { createMemo, createResource, createSignal, For, Show } from "solid-js";
import type { Monitor, MonitorDef, MonitorDirection, MonitorKind, MonitorRunReport } from "../../api/types";
import { ConfirmButton, Empty, ErrorBanner, Modal } from "../../components/ui";
import { previewCron } from "../../lib/cron";
import { ago } from "../../lib/format";
import { everyWords, parseEvery } from "../../lib/schedule";
import { api } from "../../stores";
import { StateBadge } from "./blocks";
import { blockState, scheduleText, unitOf, valueText } from "./model";

const KINDS: readonly { id: MonitorKind; label: string; help: string }[] = [
  { id: "number", label: "Number", help: "The first number on stdout (1,234.5, -3, 42%). The warn and crit thresholds give the status." },
  { id: "status", label: "Status", help: "The exit code: 0 ok, 1 warning, 2 critical, any other code unknown. The first line of stdout is the message." },
  { id: "text", label: "Text", help: "Stdout as text (at most 8 KiB). The exit code gives the status, as for Status." },
  { id: "json", label: "JSON", help: 'One JSON object: {"value", "unit", "status", "message", "rows": [{...}]}. At most 200 rows of 20 columns.' },
];

/** The fields of the dialog, as text. */
interface Form {
  name: string;
  description: string;
  target: string;
  script: string;
  mode: "interval" | "cron";
  every: string;
  cron: string;
  tz: string;
  timeout: string;
  kind: MonitorKind;
  unit: string;
  warn: string;
  crit: string;
  direction: MonitorDirection;
  retention: string;
  enabled: boolean;
}

const formOf = (m?: Monitor): Form => ({
  name: m?.name ?? "",
  description: m?.description ?? "",
  target: m?.target ?? "",
  script: m?.script ?? "",
  mode: m?.schedule.kind === "cron" ? "cron" : "interval",
  every: m?.schedule.kind === "interval" ? m.schedule.every : "1m",
  cron: m?.schedule.kind === "cron" ? m.schedule.expr : "*/5 * * * *",
  tz: m?.schedule.kind === "cron" ? m.schedule.tz : "UTC",
  timeout: String(m?.timeout_secs ?? 30),
  kind: m?.kind ?? "number",
  unit: m?.unit ?? "",
  warn: m?.warn === null || m?.warn === undefined ? "" : String(m.warn),
  crit: m?.crit === null || m?.crit === undefined ? "" : String(m.crit),
  direction: m?.direction ?? "above",
  retention: String(m?.retention_days ?? 7),
  enabled: m?.enabled ?? true,
});

const num = (s: string): number | null => (s.trim() === "" || !Number.isFinite(Number(s)) ? null : Number(s));

/** The body of a create or an update. */
export function defOf(f: Form): MonitorDef {
  const hasValue = f.kind === "number" || f.kind === "json";
  return {
    name: f.name.trim(),
    description: f.description.trim() || null,
    target: f.target,
    script: f.script,
    schedule: f.mode === "interval" ? { kind: "interval", every: f.every.trim() } : { kind: "cron", expr: f.cron.trim(), tz: f.tz.trim() || "UTC" },
    timeout_secs: Number(f.timeout) || 30,
    kind: f.kind,
    unit: hasValue ? f.unit.trim() || null : null,
    warn: hasValue ? num(f.warn) : null,
    crit: hasValue ? num(f.crit) : null,
    direction: f.direction,
    enabled: f.enabled,
    retention_days: Number(f.retention) || 7,
  };
}

export function MonitorDialog(props: { project: string; monitor?: Monitor; onClose: () => void; onSaved: (m: Monitor) => void }) {
  const [f, setF] = createSignal<Form>(formOf(props.monitor));
  const set = <K extends keyof Form>(k: K, v: Form[K]) => setF((x) => ({ ...x, [k]: v }));
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const [test, setTest] = createSignal<MonitorRunReport | null>(null);
  const [targets] = createResource(async () => (await api.targets.list()).filter((t) => (!t.project || t.project === props.project) && t.kind !== "vnc"));
  const hasValue = () => f().kind === "number" || f().kind === "json";
  const when = () => {
    const x = f();
    if (x.mode === "interval") {
      const s = parseEvery(x.every);
      if (s === null) return "Not an interval: use 15s, 1m, 5m, or 1h.";
      if (s < 15) return "The shortest interval is 15 s.";
      return `Runs every ${everyWords(s)}.`;
    }
    const p = previewCron(x.cron, x.tz || "UTC", Date.now(), 2);
    return "error" in p ? p.error : `Next runs: ${p.runs.map((r) => new Date(r).toLocaleString()).join(", ")}.`;
  };
  const runTest = async () => {
    setBusy(true);
    setError(null);
    setTest(null);
    try {
      setTest(await api.monitors.test(props.project, defOf(f())));
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };
  const save = async (e: Event) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const d = defOf(f());
      const m = props.monitor ? await api.monitors.update(props.monitor.id, d) : await api.monitors.create(props.project, d);
      props.onSaved(m);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal title={props.monitor ? `Edit monitor ${props.monitor.name}` : "New monitor"} onClose={() => props.onClose()} class="modal-wide modal-full">
      <form class="stack mon-form" onSubmit={save}>
        <div class="mon-form-grid">
          <label>
            Name
            <input required value={f().name} onInput={(e) => set("name", e.currentTarget.value)} placeholder="disk-root" pattern="[a-z0-9][a-z0-9_\-]{0,62}" title="1-63 of a-z, 0-9, - and _" />
          </label>
          <label>
            Target
            <select required value={f().target} onChange={(e) => set("target", e.currentTarget.value)}>
              <option value="">Pick a target</option>
              <For each={targets() ?? []}>
                {(t) => (
                  <option value={t.name} selected={t.name === f().target}>
                    {t.name} ({t.kind}
                    {t.project ? `, ${t.project}` : ", global"})
                  </option>
                )}
              </For>
            </select>
          </label>
          <label class="span-2">
            Description
            <input value={f().description} onInput={(e) => set("description", e.currentTarget.value)} placeholder="What this monitor proves" />
          </label>
        </div>
        <label>
          Script <span class="muted small">(runs with sh -c on the target)</span>
          <textarea class="mono mon-script" rows={7} spellcheck={false} required value={f().script} onInput={(e) => set("script", e.currentTarget.value)} placeholder="df -P / | awk 'NR==2 {print $5}'" />
        </label>
        <div class="mon-form-grid">
          <label>
            Schedule
            <select value={f().mode} onChange={(e) => set("mode", e.currentTarget.value as Form["mode"])}>
              <option value="interval">Interval</option>
              <option value="cron">Cron</option>
            </select>
          </label>
          <Show
            when={f().mode === "interval"}
            fallback={
              <>
                <label>
                  Cron expression
                  <input class="mono" value={f().cron} onInput={(e) => set("cron", e.currentTarget.value)} />
                </label>
                <label>
                  Zone
                  <input value={f().tz} onInput={(e) => set("tz", e.currentTarget.value)} placeholder="UTC" />
                </label>
              </>
            }
          >
            <label>
              Every
              <input value={f().every} onInput={(e) => set("every", e.currentTarget.value)} placeholder="1m" />
            </label>
          </Show>
          <label>
            Timeout (s)
            <input type="number" min="1" max="600" value={f().timeout} onInput={(e) => set("timeout", e.currentTarget.value)} />
          </label>
          <label>
            Keep results (days)
            <input type="number" min="1" max="90" value={f().retention} onInput={(e) => set("retention", e.currentTarget.value)} />
          </label>
        </div>
        <p class="muted small">{when()}</p>
        <div class="mon-form-grid">
          <label>
            Output kind
            <select value={f().kind} onChange={(e) => set("kind", e.currentTarget.value as MonitorKind)}>
              <For each={KINDS}>{(k) => <option value={k.id}>{k.label}</option>}</For>
            </select>
          </label>
          <Show when={hasValue()}>
            <label>
              Unit
              <input value={f().unit} onInput={(e) => set("unit", e.currentTarget.value)} placeholder="%" />
            </label>
            <label>
              Warn at
              <input type="number" step="any" value={f().warn} onInput={(e) => set("warn", e.currentTarget.value)} />
            </label>
            <label>
              Critical at
              <input type="number" step="any" value={f().crit} onInput={(e) => set("crit", e.currentTarget.value)} />
            </label>
            <label>
              Bad when the value is
              <select value={f().direction} onChange={(e) => set("direction", e.currentTarget.value as MonitorDirection)}>
                <option value="above">at or above</option>
                <option value="below">at or below</option>
              </select>
            </label>
          </Show>
        </div>
        <p class="muted small">{KINDS.find((k) => k.id === f().kind)?.help}</p>
        <label class="row small">
          <input type="checkbox" checked={f().enabled} onChange={(e) => set("enabled", e.currentTarget.checked)} /> On: run on the schedule
        </label>
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <Show when={test()}>{(t) => <RunReport report={t()} unit={f().unit || undefined} title="Test run (not saved)" />}</Show>
        <div class="row" style={{ "justify-content": "flex-end", "flex-wrap": "wrap" }}>
          <button type="button" disabled={busy() || !f().target || !f().script.trim()} onClick={runTest}>
            {busy() ? "Running…" : "Test run"}
          </button>
          <button type="button" class="ghost" onClick={() => props.onClose()}>
            Cancel
          </button>
          <button type="submit" class="primary" disabled={busy()}>
            {props.monitor ? "Save" : "Create monitor"}
          </button>
        </div>
      </form>
    </Modal>
  );
}

/** The parsed result and the raw output of a run now or a test run. */
export function RunReport(props: { report: MonitorRunReport; unit?: string; title: string }) {
  const r = () => props.report.result;
  return (
    <section class="mon-report" aria-label={props.title}>
      <div class="spread">
        <strong class="small">{props.title}</strong>
        <span class="muted small">{r().duration_ms} ms</span>
      </div>
      <div class="row" style={{ "flex-wrap": "wrap", gap: "8px" }}>
        <StateBadge state={r().status} />
        <Show when={r().value !== undefined}>
          <span>
            Value <strong>{valueText(r().value, undefined, r().json?.unit ?? props.unit)}</strong>
          </span>
        </Show>
        <Show when={r().json?.rows}>{(rows) => <span class="small">{rows().length} rows</span>}</Show>
      </div>
      <Show when={r().text}>{(t) => <div class="small">Message: {t().split("\n")[0]}</div>}</Show>
      <Show when={r().error}>{(e) => <div class="small mon-err">Error: {e()}</div>}</Show>
      <Show when={props.report.output} fallback={<div class="muted small">The script did not start.</div>}>
        {(o) => (
          <>
            <div class="small">Exit code {o().exit_code ?? "none"}</div>
            <div class="small muted">stdout</div>
            <pre class="dash-text">{o().stdout || "(empty)"}</pre>
            <Show when={o().stderr}>
              <div class="small muted">stderr</div>
              <pre class="dash-text">{o().stderr}</pre>
            </Show>
          </>
        )}
      </Show>
    </section>
  );
}

export function MonitorsPanel(props: { project: string; monitors: Monitor[]; now: number; onChanged: () => void }) {
  const [editing, setEditing] = createSignal<Monitor | "new" | null>(null);
  const [error, setError] = createSignal<unknown>(null);
  const [report, setReport] = createSignal<{ id: string; report: MonitorRunReport } | null>(null);
  const [running, setRunning] = createSignal<string | null>(null);
  const sorted = createMemo(() => [...props.monitors].sort((a, b) => a.name.localeCompare(b.name)));
  const act = async (fn: () => Promise<unknown>) => {
    setError(null);
    try {
      await fn();
      props.onChanged();
    } catch (e) {
      setError(e);
    }
  };
  const runNow = (m: Monitor) =>
    act(async () => {
      setRunning(m.id);
      try {
        setReport({ id: m.id, report: await api.monitors.run(m.id) });
      } finally {
        setRunning(null);
      }
    });
  return (
    <section aria-label="Monitors">
      <div class="spread" style={{ "margin-bottom": "8px", "flex-wrap": "wrap", gap: "8px" }}>
        <p class="muted small" style={{ margin: 0 }}>
          A monitor runs a script on a target on a schedule. Agents add them with <code>terrarium monitor new</code>.
        </p>
        <button class="primary" onClick={() => setEditing("new")}>
          New monitor
        </button>
      </div>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={sorted().length} fallback={<Empty>No monitors yet. Make one to show values on the dashboard.</Empty>}>
        <div class="table-wrap">
          <table class="mon-table">
            <thead>
              <tr>
                <th scope="col">Monitor</th>
                <th scope="col">State</th>
                <th scope="col">Last value</th>
                <th scope="col">Last run</th>
                <th scope="col">Next run</th>
                <th scope="col">On</th>
                <th scope="col">
                  <span class="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              <For each={sorted()}>
                {(m) => (
                  <>
                    <tr>
                      <td>
                        <div style={{ "font-weight": 600 }}>{m.name}</div>
                        <div class="muted small">
                          {m.kind} · {m.target} · {scheduleText(m.schedule)}
                          <Show when={m.skipped > 0}> · {m.skipped} skipped</Show>
                        </div>
                      </td>
                      <td>
                        <StateBadge state={blockState(m, props.now)} />
                      </td>
                      <td class={m.last?.value !== undefined ? "num" : ""}>{m.last?.value !== undefined ? valueText(m.last.value, undefined, unitOf(m)) : <span class="muted small">{m.last?.text?.split("\n")[0]?.slice(0, 40) ?? "–"}</span>}</td>
                      <td class="small" data-label="Last run">{m.running ? "running now" : m.last ? ago(m.last.at, props.now) : "never"}</td>
                      <td class="small" data-label="Next run">{m.enabled && m.next_run_at ? new Date(m.next_run_at).toLocaleTimeString() : "–"}</td>
                      <td data-label="On">
                        <label class="switch">
                          <input type="checkbox" checked={m.enabled} aria-label={`Run ${m.name} on its schedule`} onChange={(e) => void act(() => api.monitors.update(m.id, { enabled: e.currentTarget.checked }))} />
                        </label>
                      </td>
                      <td>
                        <div class="row mon-actions">
                          <button class="small" disabled={running() === m.id || m.running} onClick={() => void runNow(m)}>
                            {running() === m.id ? "Running…" : "Run now"}
                          </button>
                          <button class="small" onClick={() => setEditing(m)}>
                            Edit
                          </button>
                          <ConfirmButton class="small danger" label="Delete" onConfirm={() => act(() => api.monitors.remove(m.id))} />
                        </div>
                      </td>
                    </tr>
                    <Show when={report()?.id === m.id && report()}>
                      {(r) => (
                        <tr>
                          <td colSpan={7}>
                            <RunReport report={r().report} unit={unitOf(m)} title={`Run of ${m.name}`} />
                            <button class="small ghost" onClick={() => setReport(null)}>
                              Close
                            </button>
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
      </Show>
      <Show when={editing()}>
        {(e) => (
          <MonitorDialog
            project={props.project}
            monitor={e() === "new" ? undefined : (e() as Monitor)}
            onClose={() => setEditing(null)}
            onSaved={() => {
              setEditing(null);
              props.onChanged();
            }}
          />
        )}
      </Show>
    </section>
  );
}
