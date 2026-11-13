// The cog on a column head (TER-1116, kanban rework C6): the options of the
// column (docs/api.md §8.2.1). An editor for the operator and the project
// admins of the project, a viewer for everyone else: the server's
// `can_edit` decides, so a sysadmin reads only. A save sends the whole
// options with the version that was read; a `409 stale_base` asks for a
// reload, and a `422` shows each problem at its field.
//
// The rules do not run yet: the engine switch is C2 and the queue is C3.
// So the panel shows no last runs and no queue strip, and says so.

import { createMemo, createResource, createSignal, createUniqueId, For, Index, Show, type JSX } from "solid-js";
import { useInstance } from "../../instance";
import { ErrorBanner, Modal } from "../../components/ui";
import type { CatalogField, ColumnChecks, ColumnOptions, ColumnQueue, ColumnRule, QueueRelease, RuleAction, RuleCatalog, RuleWhen } from "../../api/types";
import {
  CHECK_KEYS, CHECK_TEXT, cleanRule, fieldText, fieldValue, isStale, newRule, problemsByPath, problemsUnder, putBody, queueText, ruleSentence, triggerOf,
} from "./columnRules";

/** The cog button and its panel. `column` is a column name or a role alias. */
export function ColumnCog(props: { project: string; column: string; title: string; columns?: string[] }) {
  const [open, setOpen] = createSignal(false);
  let button: HTMLButtonElement | undefined;
  return <>
    <button
      type="button"
      ref={button}
      class="ghost small column-cog"
      aria-haspopup="dialog"
      aria-label={`Options of ${props.title}`}
      title={`Options and rules of ${props.title}`}
      data-column-cog={props.column}
      onClick={(e) => { e.stopPropagation(); setOpen(true); }}
    >
      <svg viewBox="0 0 24 24" width="14" height="14" aria-hidden="true" fill="none" stroke="currentColor" stroke-width="2">
        <circle cx="12" cy="12" r="3" />
        <path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z" />
      </svg>
    </button>
    <Show when={open()}>
      <Modal title={`${props.title} options`} class="modal-full column-options-modal" onClose={() => { setOpen(false); button?.focus(); }}>
        <ColumnOptionsPanel project={props.project} column={props.column} columns={props.columns} />
      </Modal>
    </Show>
  </>;
}

/** The panel: loads the options and the catalog, then shows the editor or the viewer. */
export function ColumnOptionsPanel(props: { project: string; column: string; columns?: string[] }) {
  const { api } = useInstance();
  const [options, { mutate, refetch }] = createResource(() => [props.project, props.column] as const, ([p, c]) => api.board.options(p, c));
  // The viewer needs no catalog: a failed catalog read only stops the editor.
  // The version of the last save here: the editor says "Saved." until the next change.
  const [savedVersion, setSavedVersion] = createSignal<number>();
  const [catalog] = createResource(() => options()?.can_edit === true, (yes) => (yes ? api.rules.catalog() : Promise.resolve(undefined)));
  return <div class="stack column-options">
    <ErrorBanner error={options.error} />
    <Show when={options.loading && !options.latest}><p class="muted">Loading the options…</p></Show>
    <Show when={options.latest} keyed>
      {(o) => <Show when={o.can_edit === true} fallback={<OptionsViewer options={o} />}>
        <ErrorBanner error={catalog.error} />
        <Show when={catalog.latest} keyed fallback={<Show when={!catalog.error}><p class="muted">Loading the rule catalog…</p></Show>}>
          {(cat) => <OptionsEditor options={o} catalog={cat} columns={props.columns ?? []} saved={savedVersion() === o.version}
            onSaved={(next) => { setSavedVersion(next.version); mutate(next); }} onReload={() => { setSavedVersion(undefined); void refetch(); }} />}
        </Show>
      </Show>}
    </Show>
  </div>;
}

/** Neither last runs nor the queue strip exist before C2 and C3. */
function NotYet() {
  return <p class="small muted column-options-later">
    Last runs and the queue strip come with the new rule engine and the column queues (kanban rework C2 and C3).
    Until then the board runs the rules of the Rules tab.
  </p>;
}

function OptionsViewer(props: { options: ColumnOptions }) {
  const checks = () => CHECK_KEYS.filter((k) => props.options.checks[k]);
  return <div class="stack" data-options-view>
    <p class="small muted">You can read these options. The operator and the project admins of {props.options.project} change them.</p>
    <section aria-label="Checks">
      <h3>Checks</h3>
      <Show when={checks().length} fallback={<p class="muted">No checks.</p>}>
        <ul><For each={checks()}>{(k) => <li>{CHECK_TEXT[k]}</li>}</For></ul>
      </Show>
    </section>
    <section aria-label="Queue">
      <h3>Queue</h3>
      <p>{queueText(props.options.queue)}</p>
    </section>
    <section aria-label="Rules">
      <h3>Rules</h3>
      <Show when={props.options.rules.length} fallback={<p class="muted">No rules on this column.</p>}>
        <ol class="column-rules">
          <For each={props.options.rules}>{(r) => <li data-rule={r.name}>
            <strong class="mono">{r.name}</strong> <Show when={r.enabled === false}><span class="chip">off</span></Show>
            <Show when={r.description}><div class="small muted">{r.description}</div></Show>
            <div>{ruleSentence(r)}</div>
          </li>}</For>
        </ol>
      </Show>
    </section>
    <NotYet />
    <p class="small muted">Version {props.options.version}</p>
  </div>;
}

/** A problem of the last save at `path`. */
function Problem(props: { problems: Record<string, string>; path: string }) {
  return <Show when={props.problems[props.path]}>{(m) => <span class="bad small" role="alert" data-problem={props.path}>{m()}</span>}</Show>;
}

function OptionsEditor(props: { options: ColumnOptions; catalog: RuleCatalog; columns: string[]; saved: boolean; onSaved: (o: ColumnOptions) => void; onReload: () => void }) {
  const { api } = useInstance();
  const listId = createUniqueId();
  // The props are a snapshot: the panel makes a new editor for each read (keyed).
  const start = props.options;
  const [checks, setChecks] = createSignal<ColumnChecks>({ ...start.checks });
  const [queue, setQueue] = createSignal<ColumnQueue>({ ...start.queue });
  const [rules, setRules] = createSignal<ColumnRule[]>(structuredClone(start.rules));
  const [problems, setProblems] = createSignal<Record<string, string>>({});
  const [stale, setStale] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const body = createMemo(() => putBody(start, { checks: checks(), queue: queue(), rules: rules() }));
  const dirty = () => JSON.stringify(body()) !== JSON.stringify(putBody(start, start));
  const columnWords = createMemo(() => {
    const roles = ["working", "blocked", "validating", "review", "merge_queue", "accepted"].map((r) => `@${r}`);
    return [...props.columns, ...roles];
  });

  const editRule = (i: number, f: (r: ColumnRule) => ColumnRule) => { setRules((rs) => rs.map((r, j) => (j === i ? f(structuredClone(r)) : r))); };
  const moveRule = (i: number, by: number) => setRules((rs) => {
    const j = i + by;
    if (j < 0 || j >= rs.length) return rs;
    const next = [...rs];
    [next[i], next[j]] = [next[j]!, next[i]!];
    return next;
  });

  const save = async () => {
    if (busy()) return;
    setBusy(true); setError(null); setProblems({});
    try {
      const next = await api.board.putOptions(start.project, start.column, body());
      props.onSaved(next);
    } catch (e) {
      if (isStale(e)) setStale(true);
      else {
        const p = problemsByPath(e);
        if (Object.keys(p).length) setProblems(p); else setError(e);
      }
    } finally {
      setBusy(false);
    }
  };

  return <form class="stack" data-options-edit onSubmit={(e) => { e.preventDefault(); void save(); }}>
    <datalist id={`${listId}-columns`}><For each={columnWords()}>{(c) => <option value={c} />}</For></datalist>
    <Show when={stale()}>
      <div class="notice" role="alert" data-stale>
        These options changed since you opened them. Reload to see the new version; your changes here are not saved.
        <button type="button" onClick={() => props.onReload()}>Reload</button>
      </div>
    </Show>
    <ErrorBanner error={error()} onDismiss={() => setError(null)} />
    <Show when={Object.keys(problems()).length}>
      <div class="error-banner" role="alert" data-problems><div class="stack">
        The daemon refused the save:
        <ul><For each={Object.entries(problems())}>{([p, m]) => <li><span class="mono">{p}</span>: {m}</li>}</For></ul>
      </div></div>
    </Show>

    <fieldset>
      <legend>Checks</legend>
      <For each={CHECK_KEYS}>{(k) => <label class="row">
        <input type="checkbox" checked={checks()[k]} onChange={(e) => setChecks({ ...checks(), [k]: e.currentTarget.checked })} />
        {CHECK_TEXT[k]}
      </label>}</For>
      <Problem problems={problems()} path="checks" />
    </fieldset>

    <fieldset>
      <legend>Queue</legend>
      <label>Release
        <select value={queue().release} onChange={(e) => setQueue({ ...queue(), release: e.currentTarget.value as QueueRelease })} aria-label="Queue release">
          <option value="wip">When the WIP limit has room</option>
          <option value="batch">In batches</option>
          <option value="job">One job at a time</option>
        </select>
      </label>
      <Problem problems={problems()} path="queue.release" />
      <Show when={queue().release === "batch"}>
        <For each={[["batch_size", "Cards in a batch"], ["max_wait_seconds", "Most seconds the oldest card waits"], ["max_batch_cards", "Most cards in one batch (optional)"]] as const}>{([key, label]) => <label>
          {label}
          <input type="number" min="1" value={queue()[key] ?? ""} aria-label={label} onInput={(e) => setQueue({ ...queue(), [key]: e.currentTarget.value === "" ? null : Number(e.currentTarget.value) })} />
          <Problem problems={problems()} path={`queue.${key}`} />
        </label>}</For>
      </Show>
      <p class="small muted">{queueText(queue())}</p>
    </fieldset>

    <fieldset>
      <legend>Rules</legend>
      <Problem problems={problems()} path="rules" />
      <Show when={rules().length} fallback={<p class="muted">No rules on this column.</p>}>
        <ol class="column-rules">
          <Index each={rules()}>{(rule, i) => <li data-rule={rule().name}>
            <RuleEditor
              rule={rule()}
              path={`rules[${i}]`}
              catalog={props.catalog}
              columnList={`${listId}-columns`}
              problems={problems()}
              onChange={(f) => editRule(i, f)}
            />
            <div class="row">
              <button type="button" class="small" disabled={i === 0} onClick={() => moveRule(i, -1)} aria-label={`Move ${rule().name} up`}>Up</button>
              <button type="button" class="small" disabled={i === rules().length - 1} onClick={() => moveRule(i, 1)} aria-label={`Move ${rule().name} down`}>Down</button>
              <button type="button" class="small danger" onClick={() => setRules((rs) => rs.filter((_, j) => j !== i))} aria-label={`Remove ${rule().name}`}>Remove</button>
            </div>
          </li>}</Index>
        </ol>
      </Show>
      <button type="button" onClick={() => setRules((rs) => [...rs, newRule(props.catalog, rs.map((r) => r.name))])}>Add rule</button>
    </fieldset>

    <NotYet />
    <div class="row">
      <button class="primary" type="submit" disabled={busy() || stale() || !dirty()}>{busy() ? "Saving…" : "Save"}</button>
      <Show when={props.saved && !dirty()}><span class="good small" role="status">Saved.</span></Show>
      <span class="small muted">Version {start.version}</span>
    </div>
  </form>;
}

function RuleEditor(props: { rule: ColumnRule; path: string; catalog: RuleCatalog; columnList: string; problems: Record<string, string>; onChange: (f: (r: ColumnRule) => ColumnRule) => void }) {
  const trigger = () => props.catalog.triggers.find((t) => t.kind === props.rule.trigger.kind);
  const p = (rest: string) => `${props.path}${rest}`;
  const folded = () => problemsUnder(props.problems, props.path);
  return <details open={!props.rule.id || folded().length > 0} class="column-rule">
    <summary>
      <strong class="mono">{props.rule.name || "(no name)"}</strong> <Show when={props.rule.enabled === false}><span class="chip">off</span></Show>
      <span class="small"> {ruleSentence(cleanRule(props.rule))}</span>
      <Show when={folded().length}><span class="chip bad">{folded().length} problem{folded().length === 1 ? "" : "s"}</span></Show>
    </summary>
    <div class="stack">
      <label>Name
        <input value={props.rule.name} aria-label="Rule name" onInput={(e) => { const v = e.currentTarget.value; props.onChange((r) => ({ ...r, name: v })); }} />
        <Problem problems={props.problems} path={p(".name")} />
      </label>
      <label>Description
        <input value={props.rule.description ?? ""} aria-label="Rule description" onInput={(e) => { const v = e.currentTarget.value; props.onChange((r) => ({ ...r, description: v })); }} />
        <Problem problems={props.problems} path={p(".description")} />
      </label>
      <label class="row"><input type="checkbox" checked={props.rule.enabled !== false} onChange={(e) => { const v = e.currentTarget.checked; props.onChange((r) => ({ ...r, enabled: v })); }} /> On</label>
      <Problem problems={props.problems} path={p(".id")} />

      <label>Trigger
        <select value={props.rule.trigger.kind} aria-label="Trigger" onChange={(e) => { const k = e.currentTarget.value; props.onChange((r) => ({ ...r, trigger: triggerOf(props.catalog, k), ...(k === "job_result" ? {} : { if_job: [] }) })); }}>
          <For each={props.catalog.triggers}>{(t) => <option value={t.kind} title={t.description}>{t.kind.replace(/_/g, " ")}: {t.description}</option>}</For>
        </select>
        <Problem problems={props.problems} path={p(".trigger")} />
        <Problem problems={props.problems} path={p(".trigger.kind")} />
      </label>
      <For each={trigger()?.fields ?? []}>{(f) => <FieldInput field={f} value={props.rule.trigger[f.name]} columnList={props.columnList}
        problem={props.problems[p(`.trigger.${f.name}`)]} onChange={(v) => props.onChange((r) => ({ ...r, trigger: { ...r.trigger, [f.name]: v } }))} />}</For>

      <WhenList rule={props.rule} path={p(".when")} catalog={props.catalog} problems={props.problems} onChange={props.onChange} />
      <ObjectConditions title="Card conditions" note={props.catalog.conditions.if_card.description} kind="if_card" fields={props.catalog.conditions.if_card.fields}
        rule={props.rule} path={p(".if_card")} columnList={props.columnList} problems={props.problems} onChange={props.onChange} />
      <Show when={props.rule.trigger.kind === "job_result"}>
        <ObjectConditions title="Job conditions" note={props.catalog.conditions.if_job.description} kind="if_job" fields={props.catalog.conditions.if_job.fields}
          rule={props.rule} path={p(".if_job")} columnList={props.columnList} problems={props.problems} onChange={props.onChange} />
      </Show>

      <fieldset>
        <legend>Actions, in order</legend>
        <Problem problems={props.problems} path={p(".actions")} />
        <Index each={props.rule.actions}>{(a, j) => <ActionEditor action={a()} path={p(`.actions[${j}]`)} catalog={props.catalog} columnList={props.columnList} problems={props.problems}
          onChange={(next) => props.onChange((r) => ({ ...r, actions: r.actions.map((x, k) => (k === j ? next : x)) }))}
          onRemove={() => props.onChange((r) => ({ ...r, actions: r.actions.filter((_, k) => k !== j) }))} />}</Index>
        <button type="button" class="small" onClick={() => props.onChange((r) => ({ ...r, actions: [...r.actions, { verb: props.catalog.actions[0]?.verb ?? "card.comment", args: {} }] }))}>Add action</button>
      </fieldset>

      <label>At most this many runs an hour (optional)
        <input type="number" min="1" value={props.rule.max_per_hour ?? ""} aria-label="Most runs an hour" onInput={(e) => { const v = e.currentTarget.value; props.onChange((r) => ({ ...r, max_per_hour: v === "" ? undefined : Number(v) })); }} />
        <Problem problems={props.problems} path={p(".max_per_hour")} />
      </label>
    </div>
  </details>;
}

function WhenList(props: { rule: ColumnRule; path: string; catalog: RuleCatalog; problems: Record<string, string>; onChange: (f: (r: ColumnRule) => ColumnRule) => void }) {
  const listId = createUniqueId();
  const set = (j: number, patch: Partial<RuleWhen>) => props.onChange((r) => ({ ...r, when: (r.when ?? []).map((w, k) => (k === j ? { ...w, ...patch } : w)) }));
  return <fieldset>
    <legend>Event conditions</legend>
    <p class="small muted">{props.catalog.conditions.when.description}</p>
    <datalist id={listId}><For each={props.catalog.conditions.when.paths}>{(x) => <option value={x} />}</For></datalist>
    <Index each={props.rule.when ?? []}>{(w, j) => <div class="row">
      <input list={listId} value={w().path} aria-label="Event path" placeholder="data.state" onInput={(e) => set(j, { path: e.currentTarget.value })} />
      <select value={w().op} aria-label="Operator" onChange={(e) => set(j, { op: e.currentTarget.value, value: e.currentTarget.value === "in" ? fieldValue({ name: "value", type: "list" }, fieldText(w().value)) : fieldText(w().value) })}>
        <For each={props.catalog.conditions.when.ops}>{(o) => <option value={o}>{o}</option>}</For>
      </select>
      <input value={fieldText(w().value)} aria-label="Value" onInput={(e) => set(j, { value: w().op === "in" ? fieldValue({ name: "value", type: "list" }, e.currentTarget.value) : e.currentTarget.value })} />
      <button type="button" class="small" aria-label="Remove event condition" onClick={() => props.onChange((r) => ({ ...r, when: (r.when ?? []).filter((_, k) => k !== j) }))}>Remove</button>
      <Problem problems={props.problems} path={`${props.path}[${j}]`} />
      <Problem problems={props.problems} path={`${props.path}[${j}].path`} />
      <Problem problems={props.problems} path={`${props.path}[${j}].op`} />
      <Problem problems={props.problems} path={`${props.path}[${j}].value`} />
    </div>}</Index>
    <button type="button" class="small" onClick={() => props.onChange((r) => ({ ...r, when: [...(r.when ?? []), { path: "data.", op: props.catalog.conditions.when.ops[0] ?? "eq", value: "" }] }))}>Add event condition</button>
  </fieldset>;
}

/** `if_card` or `if_job`: a list of objects; each field that is set must hold. */
function ObjectConditions(props: { title: string; note: string; kind: "if_card" | "if_job"; fields: CatalogField[]; rule: ColumnRule; path: string; columnList: string; problems: Record<string, string>; onChange: (f: (r: ColumnRule) => ColumnRule) => void }) {
  const items = () => props.rule[props.kind] ?? [];
  const set = (j: number, name: string, v: unknown) => props.onChange((r) => ({
    ...r,
    [props.kind]: (r[props.kind] ?? []).map((c, k) => {
      if (k !== j) return c;
      const next = { ...c };
      if (v === undefined || v === "" || (Array.isArray(v) && !v.length)) delete next[name]; else next[name] = v;
      return next;
    }),
  }));
  return <fieldset>
    <legend>{props.title}</legend>
    <p class="small muted">{props.note}</p>
    <Index each={items()}>{(c, j) => <div class="stack column-condition">
      <For each={props.fields}>{(f) => <FieldInput field={{ ...f, required: false }} value={c()[f.name]} columnList={props.columnList} optionalBool
        problem={props.problems[`${props.path}[${j}].${f.name}`]} onChange={(v) => set(j, f.name, v)} />}</For>
      <Problem problems={props.problems} path={`${props.path}[${j}]`} />
      <button type="button" class="small" onClick={() => props.onChange((r) => ({ ...r, [props.kind]: (r[props.kind] ?? []).filter((_, k) => k !== j) }))}>Remove condition</button>
    </div>}</Index>
    <button type="button" class="small" onClick={() => props.onChange((r) => ({ ...r, [props.kind]: [...(r[props.kind] ?? []), {}] }))}>Add {props.title.toLowerCase().replace(/s$/, "")}</button>
  </fieldset>;
}

function ActionEditor(props: { action: RuleAction; path: string; catalog: RuleCatalog; columnList: string; problems: Record<string, string>; onChange: (a: RuleAction) => void; onRemove: () => void }) {
  const verb = () => props.catalog.actions.find((v) => v.verb === props.action.verb);
  return <div class="stack column-action">
    <div class="row">
      <select value={props.action.verb} aria-label="Action" onChange={(e) => props.onChange({ verb: e.currentTarget.value, args: {} })}>
        <For each={props.catalog.actions}>{(v) => <option value={v.verb}>{v.verb}</option>}</For>
      </select>
      <button type="button" class="small" aria-label={`Remove action ${props.action.verb}`} onClick={() => props.onRemove()}>Remove</button>
    </div>
    <Problem problems={props.problems} path={props.path} />
    <Problem problems={props.problems} path={`${props.path}.verb`} />
    <Problem problems={props.problems} path={`${props.path}.args`} />
    <For each={verb()?.args ?? []}>{(f) => <FieldInput field={f} value={props.action.args?.[f.name]} columnList={props.columnList}
      problem={props.problems[`${props.path}.args.${f.name}`]} onChange={(v) => props.onChange({ ...props.action, args: { ...(props.action.args ?? {}), [f.name]: v } })} />}</For>
  </div>;
}

/** One field of the catalog. `optionalBool`: a bool of a condition may be unset. */
function FieldInput(props: { field: CatalogField; value: unknown; columnList: string; problem?: string; optionalBool?: boolean; onChange: (v: unknown) => void }) {
  const label = () => `${props.field.name.replace(/_/g, " ")}${props.field.required ? "" : " (optional)"}`;
  const control = (): JSX.Element => {
    const f = props.field;
    switch (f.type) {
      case "bool":
        return props.optionalBool
          ? <select value={props.value === undefined ? "" : String(props.value)} aria-label={f.name} onChange={(e) => props.onChange(e.currentTarget.value === "" ? undefined : e.currentTarget.value === "true")}>
            <option value="">any</option><option value="true">yes</option><option value="false">no</option>
          </select>
          : <input type="checkbox" checked={props.value === true} aria-label={f.name} onChange={(e) => props.onChange(e.currentTarget.checked)} />;
      case "one_of":
        return <select value={fieldText(props.value)} aria-label={f.name} onChange={(e) => props.onChange(e.currentTarget.value || undefined)}>
          <Show when={!f.required}><option value="">none</option></Show>
          <For each={f.values ?? []}>{(v) => <option value={v}>{v}</option>}</For>
        </select>;
      case "one_of_list": {
        const chosen = () => (Array.isArray(props.value) ? props.value.map(String) : []);
        return <span class="row" role="group" aria-label={f.name}>
          <For each={f.values ?? []}>{(v) => <label class="row small">
            <input type="checkbox" checked={chosen().includes(v)} onChange={(e) => { const on = e.currentTarget.checked; props.onChange((f.values ?? []).filter((x) => (x === v ? on : chosen().includes(x)))); }} />
            {v}
          </label>}</For>
        </span>;
      }
      case "count":
        return <input type="number" min={f.min} max={f.max} value={fieldText(props.value)} aria-label={f.name} onInput={(e) => props.onChange(fieldValue(f, e.currentTarget.value))} />;
      case "column":
        return <input list={props.columnList} value={fieldText(props.value)} aria-label={f.name} placeholder="@accepted or a column name" onInput={(e) => props.onChange(e.currentTarget.value)} />;
      case "code":
        return <textarea value={fieldText(props.value)} aria-label={f.name} rows={3} onInput={(e) => props.onChange(e.currentTarget.value)} />;
      case "list":
      case "columns":
        return <input value={fieldText(props.value)} aria-label={f.name} placeholder="a, b" onChange={(e) => props.onChange(fieldValue(f, e.currentTarget.value))} />;
      default:
        return <input value={fieldText(props.value)} aria-label={f.name} onInput={(e) => props.onChange(e.currentTarget.value)} />;
    }
  };
  return <div class="column-field" data-field={props.field.name}>
    <span class="small">{label()}</span>
    {control()}
    <Show when={props.problem}>{(m) => <span class="bad small" role="alert">{m()}</span>}</Show>
  </div>;
}
