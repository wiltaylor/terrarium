import { useInstance } from "../../instance";
// The create and edit dialog of a schedule or a trigger (docs/api.md §19,
// §19.4). The state is in ./form; this file only draws it. A `403` and a
// `422` show inline: the `422` under the field that the daemon names. A
// trigger has the spec `on` (./TriggerFields) and runs only mail, exec,
// card, or webhook (./WebhookFields).

import { createMemo, createResource, For, Index, Show } from "solid-js";
import { ErrorBanner, Modal } from "../../components/ui";
import type { AgentVerb, CardOp, CatchUp, Schedule, ScheduleActionType, ScheduleSpecKind } from "../../api/types";

import { DAY_START_WEEKDAYS, everyWords, parseEvery, utcText, WEEKDAYS } from "../../lib/schedule";
import { actionTypesFor, agentOptions, CRON_PRESETS, createScheduleDialog, GLOBAL, targetOptions, timeZones, type CronPreset, type ScheduleOutcome } from "./form";
import { fieldKit } from "./fields";
import { TemplateHelp, TriggerSpecFields } from "./TriggerFields";
import { WebhookActionFields } from "./WebhookFields";

const SPEC_KINDS: readonly { id: ScheduleSpecKind; label: string }[] = [
  { id: "cron", label: "Cron" },
  { id: "interval", label: "Interval" },
  { id: "at", label: "Once, at a time" },
  { id: "day_start", label: "Start of day: once a day, at the first start" },
  { id: "manual", label: "On demand: a button runs it (project action)" },
];

const localZone = (() => {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone;
  } catch {
    return undefined;
  }
})();

export function ScheduleDialog(props: { project: string | null; schedule?: Schedule; trigger?: boolean; action?: boolean; onClose: () => void; onSaved: (s: Schedule) => void }) {
  const { api, agents, projects, subscriptions } = useInstance();
  // The dialog reads its props once: a new schedule needs a new dialog.
  const init = props.schedule ? undefined : props.trigger ? { specKind: "on" as const } : props.action ? { specKind: "manual" as const } : undefined;
  const dlg = createScheduleDialog(api, { project: props.project, schedule: props.schedule, init });
  const f = dlg.form;
  const trigger = dlg.isTrigger();
  const noun = trigger ? "trigger" : "schedule";
  const [targets] = createResource(() => api.targets.list().catch(() => []));
  const zones = timeZones(localZone);
  const preview = createMemo(() => (f().specKind === "cron" ? dlg.preview(Date.now()) : null));
  const forbidden = () => {
    const o = dlg.outcome();
    return o.kind === "forbidden" ? (o as Extract<ScheduleOutcome, { kind: "forbidden" }>) : null;
  };
  /** A refusal with no field of the form: a banner at the end. */
  const loose = () => {
    const o = dlg.outcome();
    return (o.kind === "invalid" || o.kind === "rejected") && !o.field ? o : null;
  };
  const submit = async (e: Event) => {
    e.preventDefault();
    const s = await dlg.submit();
    if (s) props.onSaved(s);
  };

  const kit = fieldKit(dlg);
  const { field, text, recipient } = kit;
  /** The first agent of the scope's project, for the preset. */
  const firstAgent = () => agentOptions(agents.state.agents, f().scope)[0]?.value;

  const agentPicker = () =>
    field(
      "agent",
      "Agent",
      <select aria-label="Agent" value={f().agent} onChange={(e) => dlg.set("agent", e.currentTarget.value)}>
        <option value="" disabled selected={!f().agent}>
          {f().scope === GLOBAL ? "Pick any agent" : "Pick an agent of the project"}
        </option>
        <For each={agentOptions(agents.state.agents, f().scope)}>
          {(o) => (
            <option value={o.value} selected={o.value === f().agent}>
              {o.label}
            </option>
          )}
        </For>
      </select>,
    );

  return (
    <Modal title={props.schedule ? `Edit ${noun} ${props.schedule.name}` : `New ${noun}`} onClose={() => props.onClose()}>
      <form class="stack" onSubmit={submit} aria-label={props.schedule ? `Edit ${noun}` : `New ${noun}`}>
        <Show when={trigger}>
          <div class="row small" style={{ gap: "8px", "flex-wrap": "wrap" }}>
            <button type="button" class="small" onClick={() => dlg.ciPreset(firstAgent())} title="Kinds ci.run.failed on main; dedupe per repo in 10m; escalate to the operator after 3 in a row; mail an agent">
              Preset: CI failure → mail an agent
            </button>
            <span class="muted">Fills the form; change any field before you save.</span>
          </div>
        </Show>
        <div class="form-grid">
          {text("name", "Name", trigger ? "ci-failed" : "standup")}
          {field(
            "scope",
            "Scope",
            <select aria-label="Scope" value={f().scope} onChange={(e) => dlg.set("scope", e.currentTarget.value)} required disabled={!!props.schedule} title={props.schedule ? `The scope of a ${noun} does not change` : undefined}>
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
                {trigger ? "Global: sees the events of every project (needs sysadmin)" : "Global (needs sysadmin)"}
              </option>
            </select>,
          )}
          <Show when={!trigger}>
            {field(
              "specKind",
              "When",
              <select aria-label="When" value={f().specKind} onChange={(e) => dlg.set("specKind", e.currentTarget.value as ScheduleSpecKind)}>
                <For each={SPEC_KINDS}>
                  {(k) => (
                    <option value={k.id} selected={k.id === f().specKind}>
                      {k.label}
                    </option>
                  )}
                </For>
              </select>,
            )}
          </Show>
        </div>
        <Show when={trigger}>
          <TriggerSpecFields dlg={dlg} kit={kit} />
        </Show>

        <Show when={f().specKind === "cron"}>
          <fieldset class="stack cron-helper" aria-label="Cron helper">
            <legend class="small">Cron</legend>
            <div class="form-grid">
              {field(
                "preset",
                "Preset",
                <select aria-label="Preset" value={f().preset} onChange={(e) => dlg.set("preset", e.currentTarget.value as CronPreset)}>
                  <For each={CRON_PRESETS}>
                    {(p) => (
                      <option value={p.id} selected={p.id === f().preset}>
                        {p.label}
                      </option>
                    )}
                  </For>
                </select>,
              )}
              <Show when={f().preset === "hourly"}>
                {field("minute", "Minute", <input type="number" min="0" max="59" value={f().minute} onInput={(e) => dlg.set("minute", Math.max(0, Math.min(59, Number(e.currentTarget.value) || 0)))} />)}
              </Show>
              <Show when={f().preset === "weekly"}>
                {field(
                  "weekday",
                  "Day",
                  <select aria-label="Day" value={String(f().weekday)} onChange={(e) => dlg.set("weekday", Number(e.currentTarget.value))}>
                    <For each={WEEKDAYS}>
                      {(d) => (
                        <option value={String(d.value)} selected={d.value === f().weekday}>
                          {d.label}
                        </option>
                      )}
                    </For>
                  </select>,
                )}
              </Show>
              <Show when={f().preset === "daily" || f().preset === "weekly"}>
                {field("time", "Time (HH:MM)", <input type="time" value={f().time} onInput={(e) => dlg.set("time", e.currentTarget.value)} />)}
              </Show>
              {field("expr", "Expression", <input class="mono" aria-label="Cron expression" value={f().expr} placeholder="*/15 * * * *" onInput={(e) => dlg.set("expr", e.currentTarget.value)} />, () => "minute hour day-of-month month day-of-week")}
              {field(
                "tz",
                "Time zone",
                <>
                  <input aria-label="Time zone" list="schedule-tz" value={f().tz} onInput={(e) => dlg.set("tz", e.currentTarget.value)} />
                  <datalist id="schedule-tz">
                    <Index each={zones}>{(z) => <option value={z()} />}</Index>
                  </datalist>
                </>,
              )}
            </div>
            <div class="small" aria-label="Next runs" aria-live="polite">
              <Show
                when={preview() && "runs" in preview()! ? (preview() as { runs: number[] }).runs : null}
                fallback={<span class="chip warn" title="The client preview; the daemon has the last word">{(preview() as { error?: string } | null)?.error ?? ""}</span>}
              >
                {(runs) => (
                  <>
                    <strong>Next 3 runs</strong>{" "}
                    <For each={runs()}>
                      {(ms) => (
                        <span class="chip mono" title={new Date(ms).toISOString()}>
                          {new Date(ms).toLocaleString(undefined, { weekday: "short", year: "numeric", month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", timeZone: f().tz || "UTC", timeZoneName: "short" })}
                        </span>
                      )}
                    </For>
                  </>
                )}
              </Show>
            </div>
          </fieldset>
        </Show>
        <Show when={f().specKind === "interval"}>
          <div class="form-grid">
            {text("every", "Every", "15m", () => {
              const s = parseEvery(f().every);
              return s === null ? "A duration: 15m, 1h, 1h30m, 2d" : `every ${everyWords(s)}`;
            })}
          </div>
        </Show>
        <Show when={f().specKind === "at"}>
          <div class="form-grid">
            {field("at", "At (your time)", <input type="datetime-local" aria-label="At" value={f().at} onInput={(e) => dlg.set("at", e.currentTarget.value)} />, () => (f().at && !Number.isNaN(new Date(f().at).getTime()) ? `sent as ${utcText(new Date(f().at).toISOString())}` : ""))}
          </div>
        </Show>

        <Show when={f().specKind === "day_start"}>
          {/* TER-458: one run per day in the zone, at the first start (daemon start, resume, host agent reconnect, day change). */}
          <fieldset class="stack" aria-label="Start of day">
            <legend class="small">Start of day</legend>
            <div class="form-grid">
              {field("notBefore", "Not before (HH:MM)", <input type="time" aria-label="Not before" value={f().notBefore} onInput={(e) => dlg.set("notBefore", e.currentTarget.value)} />, () => "A start before it runs at this time")}
              {field("notAfter", "Not after (HH:MM)", <input type="time" aria-label="Not after" value={f().notAfter} onInput={(e) => dlg.set("notAfter", e.currentTarget.value)} />, () => "A start after it plans no run that day")}
              {text("delay", "Delay after the start", "5m", () => {
                const s = f().delay.trim() ? parseEvery(f().delay) : null;
                return s === null ? "Empty, or a duration up to 12h: 5m, 1h30m" : `${everyWords(s)} after the start`;
              })}
              {field(
                "tz",
                "Time zone",
                <>
                  <input aria-label="Time zone" list="schedule-tz-day" value={f().tz} onInput={(e) => dlg.set("tz", e.currentTarget.value)} />
                  <datalist id="schedule-tz-day">
                    <Index each={zones}>{(z) => <option value={z()} />}</Index>
                  </datalist>
                </>,
              )}
            </div>
            {field(
              "weekdays",
              "Days (none: every day)",
              <span class="row small" style={{ "flex-wrap": "wrap", gap: "8px" }}>
                <For each={DAY_START_WEEKDAYS}>
                  {(d) => (
                    <label class="row small">
                      <input
                        type="checkbox"
                        aria-label={`On ${d}`}
                        checked={f().weekdays.includes(d)}
                        onChange={(e) => dlg.set("weekdays", e.currentTarget.checked ? [...f().weekdays.filter((x) => x !== d), d] : f().weekdays.filter((x) => x !== d))}
                      />
                      {d[0]!.toUpperCase() + d.slice(1)}
                    </label>
                  )}
                </For>
              </span>,
            )}
          </fieldset>
        </Show>

        <div class="form-grid">
          {field(
            "actionType",
            "Action",
            <select aria-label="Action" value={f().actionType} onChange={(e) => dlg.set("actionType", e.currentTarget.value as ScheduleActionType)}>
              <For each={actionTypesFor(f().specKind)}>
                {(a) => (
                  <option value={a.id} selected={a.id === f().actionType}>
                    {a.label}
                  </option>
                )}
              </For>
            </select>,
          )}
          <Show when={f().actionType === "mail"}>
            {recipient("mailTo", "To", projects.projects(), agents.state.agents, "Pick a recipient")}
            {text("mailSubject", "Subject", trigger ? "CI failed: {{event.data.repo}}" : "Standup")}
          </Show>
          <Show when={f().actionType === "card"}>
            {field(
              "cardOp",
              "Card",
              <select aria-label="Card" value={f().cardOp} onChange={(e) => dlg.set("cardOp", e.currentTarget.value as CardOp)}>
                <option value="open" selected={f().cardOp === "open"}>
                  open (or comment on the open card)
                </option>
                <option value="close" selected={f().cardOp === "close"}>
                  close (move the open card)
                </option>
              </select>,
            )}
            {text("cardKey", "Key", trigger ? "ci:{{event.data.repo}}" : "weekly-review", () => "One open card per key; the card has the label trigger:<key>.")}
            <Show when={f().cardOp === "open"}>{text("cardTitle", "Title", trigger ? "CI failed on {{event.data.repo}}" : "Weekly review")}</Show>
            {text("cardColumn", "Column", f().cardOp === "open" ? "backlog" : "done", () => (f().cardOp === "open" ? "Empty: backlog" : "Empty: done"))}
          </Show>
          <Show when={f().actionType === "turn" || f().actionType === "agent"}>{agentPicker()}</Show>
          <Show when={f().actionType === "agent"}>
            {field(
              "verb",
              "Verb",
              <select aria-label="Verb" value={f().verb} onChange={(e) => dlg.set("verb", e.currentTarget.value as AgentVerb)}>
                <For each={["start", "stop", "restart"] as const}>
                  {(v) => (
                    <option value={v} selected={v === f().verb}>
                      {v}
                    </option>
                  )}
                </For>
              </select>,
            )}
          </Show>
          <Show when={f().actionType === "exec"}>
            {field(
              "target",
              "Target",
              <select aria-label="Target" value={f().target} onChange={(e) => dlg.set("target", e.currentTarget.value)}>
                <option value="" disabled selected={!f().target}>
                  {targets.loading ? "Loading…" : "Pick a target"}
                </option>
                <For each={targetOptions(targets() ?? [], f().scope)}>
                  {(o) => (
                    <option value={o.value} selected={o.value === f().target}>
                      {o.label}
                    </option>
                  )}
                </For>
              </select>,
            )}
            {field("cmd", "Command", <input class="mono" aria-label="Command" value={f().cmd} placeholder="make test" onInput={(e) => dlg.set("cmd", e.currentTarget.value)} />, () => "An argv: quotes group words; no shell unless you write sh -c '…'")}
            {text("cwd", "Working directory", "optional")}
            {text("timeout", "Timeout (seconds)", "optional")}
          </Show>
          <Show when={f().actionType === "probe"}>
            {field(
              "subscription",
              "Subscription",
              <select aria-label="Subscription" value={f().subscription} onChange={(e) => dlg.set("subscription", e.currentTarget.value)}>
                <option value="" disabled selected={!f().subscription}>
                  Pick a subscription
                </option>
                <For each={subscriptions.list()}>
                  {(s) => (
                    <option value={s.name} selected={s.name === f().subscription}>
                      {s.name}
                      {s.probe ? "" : " (no probe)"}
                    </option>
                  )}
                </For>
              </select>,
            )}
          </Show>
        </div>
        <Show when={f().actionType === "mail"}>
          {field("mailBody", "Body", <textarea rows={3} value={f().mailBody} onInput={(e) => dlg.set("mailBody", e.currentTarget.value)} />)}
        </Show>
        <Show when={f().actionType === "webhook"}>
          <WebhookActionFields dlg={dlg} kit={kit} />
        </Show>
        <Show when={f().actionType === "card"}>
          {field("cardBody", f().cardOp === "open" ? "Body (a comment when the card is open)" : "Comment", <textarea rows={3} value={f().cardBody} onInput={(e) => dlg.set("cardBody", e.currentTarget.value)} />)}
        </Show>
        <Show when={trigger && f().actionType !== "webhook"}>
          <TemplateHelp />
        </Show>
        <Show when={f().actionType === "turn"}>
          {field("text", "Text of the turn", <textarea rows={3} value={f().text} onInput={(e) => dlg.set("text", e.currentTarget.value)} />)}
          <label class="row small">
            <input type="checkbox" checked={f().startIfStopped} onChange={(e) => dlg.set("startIfStopped", e.currentTarget.checked)} />
            start the agent first, then type the turn (also ends a usage-limit hold; without it a stopped agent wakes for the queued turn)
          </label>
        </Show>

        <div class="row" style={{ "flex-wrap": "wrap", gap: "12px" }}>
          {/* catch_up does not apply to a day_start schedule (docs/api.md §19). */}
          <label class="row small" hidden={f().specKind === "day_start"}>
            {trigger ? "Events while the daemon was down" : "Missed runs"}
            <select aria-label="Catch up" value={f().catchUp} onChange={(e) => dlg.set("catchUp", e.currentTarget.value as CatchUp)}>
              <option value="skip" selected={f().catchUp === "skip"}>
                skip them
              </option>
              <option value="once" selected={f().catchUp === "once"}>
                {trigger ? "run each of them" : "run once to catch up"}
              </option>
            </select>
          </label>
          <label class="row small">
            <input type="checkbox" checked={f().enabled} onChange={(e) => dlg.set("enabled", e.currentTarget.checked)} />
            enabled
          </label>
        </div>
        <Show when={dlg.fieldError("catchUp")}>{(m) => <span class="field-error small" role="alert">{m()}</span>}</Show>

        <Show when={forbidden()}>
          {(o) => (
            <div class="error-banner" role="alert" aria-label="Refused">
              <div class="stack" style={{ gap: "2px" }}>
                <strong>Refused: you may not write a {o().scope === GLOBAL ? `global ${noun}` : `${noun} of project ${o().scope}`}.</strong>
                <span class="small">{o().message}</span>
                <span class="small muted">{o().scope === GLOBAL ? `A global ${noun} needs the sysadmin role. Choose a project.` : `It needs project_admin of ${o().scope}; a member may only read.`}</span>
              </div>
            </div>
          )}
        </Show>
        <Show when={loose()}>
          {(o) => (
            <p class="error-banner small" role="alert">
              {o().message}
              <Show when={o().kind === "rejected" && (o() as { daemonField?: string }).daemonField}>{(fld) => <span class="mono muted"> ({fld()})</span>}</Show>
            </p>
          )}
        </Show>
        <ErrorBanner error={dlg.outcome().kind === "error" ? (dlg.outcome() as { error: unknown }).error : null} />
        <div class="row" style={{ "justify-content": "flex-end" }}>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
          <button class="primary" type="submit" disabled={dlg.outcome().kind === "sending"}>
            {dlg.outcome().kind === "sending" ? "Saving…" : props.schedule ? "Save" : "Create"}
          </button>
        </div>
      </form>
    </Modal>
  );
}
