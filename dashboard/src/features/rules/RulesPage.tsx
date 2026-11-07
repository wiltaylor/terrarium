// Rules (docs/api.md §19.8, §19.9, §19.12, TER-171): the Rules tab of the
// board screen (`/p/:project/board/rules`), beside Schedules. It shows who
// runs the card flow (`board_flow`), the lint of the rules that run, the
// rules in their order (the built-in pack and the project's rules.wcl) with
// a switch for each, the last runs, and the migration of the project to
// `board_flow = rules` (a plan first, then Apply).

import { useParams } from "@solidjs/router";
import { createEffect, createSignal, For, on, Show } from "solid-js";
import { ConfirmButton, Empty, ErrorBanner, PageHead } from "../../components/ui";
import type { EffectiveRules, RuleRun, RulesMigration, RuleFinding } from "../../api/types";
import { api } from "../../stores";
import { ago } from "../../lib/format";
import { changeText, findingText, flowText, offerMigration, ruleRows, runTone, stepsText } from "./model";

export default function RulesPage() {
  const params = useParams<{ project: string }>();
  const project = () => params.project;

  const [rules, setRules] = createSignal<EffectiveRules | null>(null);
  const [findings, setFindings] = createSignal<RuleFinding[]>([]);
  const [runs, setRuns] = createSignal<RuleRun[]>([]);
  const [plan, setPlan] = createSignal<RulesMigration | null>(null);
  const [open, setOpen] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal<string | null>(null);
  const [error, setError] = createSignal<unknown>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [loading, setLoading] = createSignal(true);
  let generation = 0;

  const load = async () => {
    const gen = ++generation;
    const p = project();
    try {
      const [eff, lint, log] = await Promise.all([api.rules.effective(p), api.rules.lint(p), api.rules.runs(p, { limit: 20 })]);
      if (gen !== generation) return;
      setRules(eff);
      setFindings(lint.findings);
      setRuns(log.runs);
      setError(null);
    } catch (e) {
      if (gen === generation) setError(e);
    } finally {
      if (gen === generation) setLoading(false);
    }
  };

  createEffect(
    on(project, () => {
      setLoading(true);
      setRules(null);
      setPlan(null);
      void load();
    }),
  );

  const guard = async (what: string, fn: () => Promise<void>) => {
    setBusy(what);
    try {
      await fn();
      setError(null);
    } catch (e) {
      setError(e);
    } finally {
      setBusy(null);
    }
  };

  const toggle = (name: string, el: HTMLInputElement) =>
    guard(`toggle:${name}`, async () => {
      try {
        const r = await (el.checked ? api.rules.enable(project(), name) : api.rules.disable(project(), name));
        setNotice(r.changed ? `${name} is ${r.enabled ? "on" : "off"}: rules.wcl changed.` : `${name} was ${r.enabled ? "on" : "off"} already.`);
        await load();
      } catch (e) {
        el.checked = !el.checked;
        throw e;
      }
    });

  const planMigration = () => guard("plan", async () => void setPlan(await api.rules.migrate(project(), false)));
  const applyMigration = () =>
    guard("apply", async () => {
      const r = await api.rules.migrate(project(), true);
      setPlan(null);
      setNotice(`${project()} runs board_flow rules${r.changed ? ": rules.wcl has the overrides" : ""}.`);
      await load();
    });

  const rows = () => ruleRows(rules()?.rules ?? []);

  return (
    <>
      <PageHead title="Rules" level={2}>
        <button onClick={() => void load()}>Refresh</button>
      </PageHead>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={notice()}>
        <p class="notice small spread" role="status">
          {notice()}
          <button class="ghost small" onClick={() => setNotice(null)} aria-label="Dismiss">
            ✕
          </button>
        </p>
      </Show>
      <Show when={rules()} fallback={<Empty>{loading() ? "Loading…" : "No rules."}</Empty>}>
        {(eff) => (
          <>
            <p class="small" aria-label="Board flow">
              {flowText(eff().board_flow)}
              <Show when={eff().source === "file"}> rules.wcl comes from a file: edit the file, then reload.</Show>
            </p>

            <section class="card-box" style={{ "margin-bottom": "16px" }} aria-label="Lint">
              <h3>Lint</h3>
              <Show when={findings().length} fallback={<p class="small muted">No findings: every column that a rule names is on the board, and a rule moves cards out of each column that rules move them into.</p>}>
                <ul>
                  <For each={findings()}>{(f) => <li class="small">{findingText(f)}</li>}</For>
                </ul>
              </Show>
            </section>

            <Show when={offerMigration(eff().board_flow, findings())}>
              <section class="card-box" style={{ "margin-bottom": "16px" }} aria-label="Migration">
                <h3>Switch to rules</h3>
                <p class="small muted">The migration maps the built-in pack to this board: a move to a column that the board lacks becomes its comment, and the wait columns map by role. It writes the overrides into rules.wcl and sets board_flow rules in one write.</p>
                <div class="row" style={{ gap: "8px" }}>
                  <button onClick={() => void planMigration()} disabled={busy() !== null}>
                    Plan the migration
                  </button>
                  <Show when={plan()}>
                    <ConfirmButton class="primary" label="Apply" confirmLabel="Switch to rules" onConfirm={applyMigration} disabled={busy() !== null || eff().source === "file"} />
                  </Show>
                </div>
                <Show when={plan()}>
                  {(m) => (
                    <div aria-label="Migration plan">
                      <Show when={m().changes.length} fallback={<p class="small">The pack fits the board: no rule needs a change.</p>}>
                        <ul>
                          <For each={m().changes}>{(c) => <li class="small">{changeText(c)}</li>}</For>
                        </ul>
                      </Show>
                      <Show when={m().changed}>
                        <pre class="small">{m().text}</pre>
                      </Show>
                    </div>
                  )}
                </Show>
              </section>
            </Show>

            <section class="card-box" style={{ "margin-bottom": "16px" }}>
              <div class="table-wrap">
                <table aria-label="Rules">
                  <thead>
                    <tr>
                      <th>Rule</th>
                      <th>From</th>
                      <th>Events</th>
                      <th>Verbs</th>
                      <th>Enabled</th>
                    </tr>
                  </thead>
                  <tbody>
                    <Show when={rows().length} fallback={<tr><td colSpan={5}><Empty>No rules run in this project.</Empty></td></tr>}>
                      <For each={rows()}>
                        {(r) => (
                          <>
                            <tr aria-label={`Rule ${r.name}`}>
                              <td>
                                <button class="ghost small disclosure" aria-expanded={open() === r.name} aria-label={`${open() === r.name ? "Close" : "Open"} the text of ${r.name}`} onClick={() => setOpen(open() === r.name ? null : r.name)}>
                                  {open() === r.name ? "▾" : "▸"}
                                </button>{" "}
                                <strong>{r.name}</strong>
                                <Show when={r.description}>
                                  <div class="small muted">{r.description}</div>
                                </Show>
                              </td>
                              <td class="small">{r.originLabel}</td>
                              <td class="small mono">{r.events}</td>
                              <td class="small mono">{r.verbs}</td>
                              <td>
                                <input type="checkbox" checked={r.enabled} aria-label={`Enable ${r.name}`} disabled={busy() !== null || eff().source === "file"} onChange={(e) => void toggle(r.name, e.currentTarget)} />
                              </td>
                            </tr>
                            <Show when={open() === r.name}>
                              <tr>
                                <td colSpan={5}>
                                  <pre class="small">{r.wcl}</pre>
                                </td>
                              </tr>
                            </Show>
                          </>
                        )}
                      </For>
                    </Show>
                  </tbody>
                </table>
              </div>
            </section>

            <section class="card-box" aria-label="Runs">
              <h3>Last runs</h3>
              <div class="table-wrap">
                <table aria-label="Rule runs">
                  <thead>
                    <tr>
                      <th>When</th>
                      <th>Rule</th>
                      <th>Event</th>
                      <th>Card</th>
                      <th>State</th>
                      <th>Steps</th>
                    </tr>
                  </thead>
                  <tbody>
                    <Show when={runs().length} fallback={<tr><td colSpan={6}><Empty>No runs yet.</Empty></td></tr>}>
                      <For each={runs()}>
                        {(run) => (
                          <tr>
                            <td class="small">{ago(run.created_at)}</td>
                            <td class="small">{run.rule}</td>
                            <td class="small mono">{run.event_seq}</td>
                            <td class="small">{run.card ?? "-"}</td>
                            <td>
                              <span class={`chip ${runTone(run.state)}`}>{run.state}</span>
                            </td>
                            <td class="small">{stepsText(run)}</td>
                          </tr>
                        )}
                      </For>
                    </Show>
                  </tbody>
                </table>
              </div>
            </section>
          </>
        )}
      </Show>
    </>
  );
}
