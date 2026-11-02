// Ephemeral agents on the agent page (docs/api.md §6.10): the Spawned tab
// of an owner, the line on the page of an ephemeral agent, and the Spawn
// rights fields of the agent form.

import { A } from "@solidjs/router";
import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import type { Agent, Spawn, SpawnPolicy } from "../../api/types";
import { api, events } from "../../stores";
import { ago } from "../../lib/format";
import { ConfirmButton, Drawer, ErrorBanner, useNow } from "../../components/ui";
import { durationText, lifetimeLeft, newSpawnPolicy, orderSpawns, resultPreview, spawnChip, spawnDuration, SPAWN_DEFAULT_CONCURRENT, SPAWN_DEFAULT_LIFETIME, templateChoices } from "./spawn";

/** The event kinds that change the Spawned list of an owner. */
const SPAWN_EVENTS = new Set(["agent.spawned", "agent.finished", "agent_removed", "status"]);

/**
 * The Spawned tab: the ephemeral agents of `owner`, running and finished,
 * newest first. A row opens a drawer with the full task and result. A
 * running one has Cancel. The session link opens `/sessions/<id>`, also
 * after the finish while the archive keeps the file.
 */
export function SpawnedPanel(props: { owner: Agent }) {
  const [error, setError] = createSignal<unknown>(null);
  const [open, setOpen] = createSignal<Spawn | null>(null);
  const [rows, { refetch }] = createResource(
    () => props.owner.id,
    (id) => api.agents.spawned(id, { limit: 100 }).then((p) => p.items),
  );
  const now = useNow(5000);
  onMount(() => {
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = events.on((e) => {
      if (!SPAWN_EVENTS.has(e.kind)) return;
      const mine = e.agent_id === props.owner.id || (rows() ?? []).some((s) => s.id === e.agent_id);
      if (!mine) return;
      clearTimeout(t);
      t = setTimeout(() => void refetch(), 300);
    });
    onCleanup(() => {
      clearTimeout(t);
      off();
    });
  });
  const cancel = async (s: Spawn) => {
    try {
      const done = await api.agents.cancel(s.id);
      setError(null);
      if (open()?.id === s.id) setOpen(done);
      void refetch();
    } catch (e) {
      setError(e);
    }
  };

  return (
    <div class="stack">
      <ErrorBanner error={error() ?? rows.error} onDismiss={() => setError(null)} />
      <p class="muted small" style={{ margin: 0 }}>
        Ephemeral agents that {props.owner.name} spawned with <code>terrarium agent spawn</code>. Each does one task, ends with <code>terrarium agent done</code>, and the result goes to {props.owner.name} by mail. The agents list does not show them.
      </p>
      <Show when={(rows() ?? []).length} fallback={<p class="muted">{rows.loading ? "Loading…" : "No ephemeral agents yet."}</p>}>
        {/* width 0 and min-width 100%: the table scrolls inside on a phone and does not widen the page. */}
        <div class="table-wrap" style={{ width: "0", "min-width": "100%", position: "relative" }}>
          <table class="spawned-table">
            <thead>
              <tr>
                <th>Status</th>
                <th>Name</th>
                <th>Template</th>
                <th>Task</th>
                <th>Result</th>
                <th>Duration</th>
                <th>Session</th>
                <th>
                  <span class="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              <For each={orderSpawns(rows() ?? [])}>
                {(s) => {
                  const chip = () => spawnChip(s);
                  return (
                    <tr class="clickable" onClick={() => setOpen(s)}>
                      <td>
                        <span class={`chip ${chip().cls === "danger" ? "bad" : chip().cls}`}>{chip().label}</span>
                      </td>
                      <td>
                        <Show when={!s.finished_at} fallback={<span class="mono">{s.name}</span>}>
                          <A href={`/agents/${encodeURIComponent(s.id)}`} class="mono" onClick={(e) => e.stopPropagation()}>
                            {s.name}
                          </A>
                        </Show>
                      </td>
                      <td class="mono small">{s.template}</td>
                      <td class="spawned-task">
                        <button
                          type="button"
                          class="link"
                          onClick={(e) => {
                            e.stopPropagation();
                            setOpen(s);
                          }}
                        >
                          {s.summary || "(no task)"}
                        </button>
                      </td>
                      <td class="spawned-result muted small">{resultPreview(s.result) || (s.finished_at ? "none" : "")}</td>
                      <td class="small" title={s.spawned_at}>
                        {durationText(spawnDuration(s, now()))}
                      </td>
                      <td>
                        <Show when={s.session_id} fallback={<span class="muted small">none</span>}>
                          {(sid) => (
                            <A href={`/sessions/${encodeURIComponent(sid())}`} onClick={(e) => e.stopPropagation()}>
                              session
                            </A>
                          )}
                        </Show>
                      </td>
                      <td onClick={(e) => e.stopPropagation()}>
                        <Show when={!s.finished_at}>
                          <ConfirmButton label="Cancel" confirmLabel="Yes, cancel" class="small" onConfirm={() => cancel(s)} />
                        </Show>
                      </td>
                    </tr>
                  );
                }}
              </For>
            </tbody>
          </table>
        </div>
      </Show>
      <Show when={open()}>{(s) => <SpawnDrawer spawn={s()} now={now()} onClose={() => setOpen(null)} onCancel={() => cancel(s())} />}</Show>
    </div>
  );
}

/** The drawer of one spawn: the full task and result, as text. */
function SpawnDrawer(props: { spawn: Spawn; now: number; onClose: () => void; onCancel: () => void }) {
  const s = () => props.spawn;
  const chip = () => spawnChip(s());
  return (
    <Drawer title={<span class="mono">{s().name}</span>} onClose={props.onClose}>
      <div class="stack">
        <dl class="kv">
          <dt>Status</dt>
          <dd>
            <span class={`chip ${chip().cls === "danger" ? "bad" : chip().cls}`}>{chip().label}</span>
          </dd>
          <dt>Template</dt>
          <dd class="mono">{s().template}</dd>
          <dt>Harness</dt>
          <dd>{s().harness}</dd>
          <dt>Spawned</dt>
          <dd title={s().spawned_at}>
            {ago(s().spawned_at)} by {s().spawned_by}
          </dd>
          <dt>Duration</dt>
          <dd>{durationText(spawnDuration(s(), props.now))}</dd>
          <Show when={!s().finished_at}>
            <dt>Lifetime</dt>
            <dd>{lifetimeLeft({ spawn: { owner_name: s().owner_name, template: s().template, task: s().task, spawned_at: s().spawned_at, deadline_at: s().deadline_at } }, props.now)}</dd>
          </Show>
          <dt>Session</dt>
          <dd>
            <Show when={s().session_id} fallback={<span class="muted">none</span>}>
              {(sid) => <A href={`/sessions/${encodeURIComponent(sid())}`}>open the session</A>}
            </Show>
          </dd>
        </dl>
        <div>
          <div class="small muted">Task</div>
          <pre class="spawn-text">{s().task}</pre>
        </div>
        <div>
          <div class="small muted">Result {s().finished_at ? "" : "(when it finishes)"}</div>
          <Show when={s().result?.trim()} fallback={<p class="muted small">none</p>}>
            <pre class="spawn-text">{s().result}</pre>
          </Show>
        </div>
        <Show when={!s().finished_at}>
          <div class="row">
            <ConfirmButton label="Cancel" confirmLabel="Yes, cancel the agent" onConfirm={() => props.onCancel()} />
          </div>
        </Show>
      </div>
    </Drawer>
  );
}

/** The line on the page of an ephemeral agent: its owner, its task, and the lifetime left. */
export function EphemeralLine(props: { agent: Agent }) {
  const now = useNow(10_000);
  const owner = () => props.agent.owner;
  return (
    <p class="notice small ephemeral-line" style={{ margin: 0 }}>
      Ephemeral agent of{" "}
      <Show when={owner() && owner() !== "operator"} fallback={<strong>the operator</strong>}>
        <A href={`/agents/${encodeURIComponent(owner()!)}?tab=spawned`}>{props.agent.spawn?.owner_name ?? owner()}</A>
      </Show>
      <Show when={props.agent.spawn}>
        {(sp) => (
          <>
            {" "}
            · <span class="mono">{sp().template}</span> · <span title={sp().task}>{resultPreview(sp().task, 1, 120)}</span> · {lifetimeLeft(props.agent, now())}
          </>
        )}
      </Show>
    </p>
  );
}

/**
 * The Spawn rights fields of the agent form (`policy.spawn`): the
 * templates, the counts, the lifetime, and the children right. The
 * daemon checks who may set them: a lead on a member of its project,
 * sysadmin and the operator on anyone, and `may spawn children` only
 * sysadmin and the operator.
 */
export function SpawnRightsFields(props: { project: string; value: SpawnPolicy | null | undefined; onChange: (v: SpawnPolicy | null) => void }) {
  const [templates] = createResource(() => api.templates.list().catch(() => []));
  const sp = () => props.value ?? null;
  const set = (patch: Partial<SpawnPolicy>) => {
    const cur = sp();
    if (cur) props.onChange({ ...cur, ...patch });
  };
  const toggle = (value: string, on: boolean) => {
    const cur = sp();
    if (!cur) return;
    const list = on ? [...new Set([...cur.templates, value])] : cur.templates.filter((t) => t !== value);
    set({ templates: list });
  };
  const num = (s: string): number | undefined => (s.trim() === "" ? undefined : Number(s));
  return (
    <fieldset class="stack" style={{ gap: "8px" }}>
      <legend>Spawn rights</legend>
      <label class="row small">
        <input type="checkbox" checked={!!sp()} onChange={(e) => props.onChange(e.currentTarget.checked ? newSpawnPolicy() : null)} /> May spawn ephemeral agents (<code>terrarium agent spawn</code>)
      </label>
      <Show when={sp()}>
        {(cur) => (
          <>
            <div>
              <div class="small">Templates</div>
              <div class="row" style={{ "flex-wrap": "wrap" }}>
                <For each={templateChoices(props.project, templates() ?? [])}>
                  {(c) => (
                    <label class="row small">
                      <input type="checkbox" checked={cur().templates.includes(c.value)} onChange={(e) => toggle(c.value, e.currentTarget.checked)} /> {c.label}
                    </label>
                  )}
                </For>
              </div>
              <Show when={!cur().templates.length}>
                <span class="muted small">No template: the agent cannot spawn until you pick one.</span>
              </Show>
            </div>
            <div class="form-grid">
              <label>
                Max concurrent
                <input type="number" min="1" max="50" value={cur().max_concurrent ?? SPAWN_DEFAULT_CONCURRENT} onInput={(e) => set({ max_concurrent: num(e.currentTarget.value) ?? SPAWN_DEFAULT_CONCURRENT })} />
              </label>
              <label>
                Max per day
                <input type="number" min="1" value={cur().max_per_day ?? ""} placeholder="no limit" onInput={(e) => set({ max_per_day: num(e.currentTarget.value) })} />
              </label>
              <label>
                Max lifetime
                <input value={cur().max_lifetime ?? ""} placeholder={SPAWN_DEFAULT_LIFETIME} onInput={(e) => set({ max_lifetime: e.currentTarget.value.trim() || undefined })} />
              </label>
            </div>
            <label class="row small">
              <input type="checkbox" checked={!!cur().may_spawn_children} onChange={(e) => set({ may_spawn_children: e.currentTarget.checked || undefined })} /> Its ephemeral agents may spawn too (one level; sysadmin or the operator)
            </label>
            <span class="muted small">A lead may set these on a member of its project. The lifetime is at most 24h; at its end the daemon stops the ephemeral agent (outcome timeout).</span>
          </>
        )}
      </Show>
    </fieldset>
  );
}
