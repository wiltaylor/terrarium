import { useInstance } from "../../instance";
// The records of one DB type (/p/:project/knowledge/db/:type): a query box
// (prefilled `type:<name>`, Enter runs it), a table with the first fields
// as columns and a column chooser, sortable headers (they write `sort:`
// terms), pages, and the record dialog. `?q=` fills the query box, `?id=`
// opens a record (the links of the wiki `db` blocks).

import { useParams, useSearchParams } from "@solidjs/router";
import { A, useNavigate } from "../../instance/navigation";
import { createEffect, createMemo, createResource, createSignal, For, on, onCleanup, Show } from "solid-js";
import type { DbQueryResult, DbRecord } from "../../api/types";
import { ConfirmButton, Empty, ErrorBanner, PageHead } from "../../components/ui";

import { cellText, columnValue, defaultColumns, errorAt, queryErrorOf, sortOf, splitTerms, toggleSort, withType } from "./model";
import { projectBase } from "../../lib/url";
import { RecordModal } from "./RecordModal";
import { SchemaEditor } from "./SchemaEditor";

type RecordSearch = { q?: string; id?: string };

export default function DbRecordsPage() {
  const params = useParams<{ project: string; type: string }>();
  const [search, setSearch] = useSearchParams<RecordSearch>();
  const navigate = useNavigate();
  return <DbRecordsContent project={params.project} typeName={decodeURIComponent(params.type)} search={search} onSearch={setSearch} onBack={() => navigate(`${projectBase(params.project)}/knowledge/db`)} />;
}

export function DbRecordsPanel(props: { project: string }) {
  return <Show when={props.project} keyed>{(project) => <RecordsSelector project={project} />}</Show>;
}

function RecordsSelector(props: { project: string }) {
  const { api } = useInstance();
  const [types] = createResource(() => props.project, (p) => api.db.types(p));
  const [name, setName] = createSignal("");
  const selected = () => types.error ? undefined : types()?.find((t) => t.name === name())?.name;
  return (
    <>
      <label class="row">Record type
        <select aria-label="Record type" value={name()} onChange={(e) => setName(e.currentTarget.value)}>
          <option value="">Choose a type</option>
          <For each={types.error ? [] : types()}>{(t) => <option value={t.name}>{t.name}</option>}</For>
        </select>
      </label>
      <ErrorBanner error={types.error} />
      <Show when={selected()} keyed fallback={<Empty>{types.loading ? "Loading…" : name() ? "Not Found" : "Choose a type."}</Empty>}>
        {(typeName) => <LocalRecords project={props.project} typeName={typeName} onBack={() => setName("")} />}
      </Show>
    </>
  );
}

function LocalRecords(props: { project: string; typeName: string; onBack: () => void }) {
  const [search, setSearch] = createSignal<RecordSearch>({});
  return <DbRecordsContent project={props.project} typeName={props.typeName} search={search()} onSearch={(patch) => setSearch((old) => ({ ...old, ...patch }))} onBack={props.onBack} />;
}

function DbRecordsContent(props: { project: string; typeName: string; search: RecordSearch; onSearch: (patch: RecordSearch) => void; onBack: () => void }) {
  const { api, events } = useInstance();
  const typeName = () => props.typeName;
  const [typeRes, { refetch: refetchType, mutate: setType }] = createResource(() => ({ p: props.project, t: typeName() }), (k) => api.db.type(k.p, k.t));
  // A resource in error throws when read: read it only without an error.
  const type = () => (typeRes.error ? undefined : typeRes());
  const [types, { refetch: refetchTypes }] = createResource(() => props.project, (p) => api.db.types(p).catch(() => []));

  // Local selection replaces search objects. Only a changed q resets query/draft.
  const searchQuery = createMemo(() => props.search.q);
  const initial = () => (searchQuery() ? withType(searchQuery()!, typeName()) : `type:${typeName()}`);
  const [query, setQuery] = createSignal(initial());
  const [draft, setDraft] = createSignal(initial());
  createEffect(
    on([() => props.typeName, searchQuery], () => {
      setQuery(initial());
      setDraft(initial());
    }, { defer: true }),
  );

  // The pages of the current query.
  const [pages, setPages] = createSignal<DbQueryResult[]>([]);
  const [queryError, setQueryError] = createSignal<unknown>(null);
  const [loading, setLoading] = createSignal(false);
  let token = 0;
  const load = async (more = false) => {
    const mine = ++token;
    const q = query();
    setLoading(true);
    try {
      const cursor = more ? pages().at(-1)?.next_cursor : undefined;
      const r = await api.db.query(props.project, q, { cursor });
      if (mine !== token) return;
      setPages((p) => (more ? [...p, r] : [r]));
      setQueryError(null);
    } catch (e) {
      if (mine !== token) return;
      setQueryError(e);
      if (!more) setPages([]);
    } finally {
      if (mine === token) setLoading(false);
    }
  };
  createEffect(on([query, () => props.project], () => void load()));

  const first = () => pages()[0];
  const records = createMemo(() => pages().flatMap((p) => p.records));
  const recordsById = createMemo(() => new Map(records().map((record) => [record.id, record])));
  const labels = createMemo(() => Object.assign({}, ...pages().map((p) => p.labels)) as Record<string, string>);
  const hasFields = () => splitTerms(query()).some((t) => t.startsWith("fields:"));

  // The column chooser: the first fields at first.
  const [chosen, setChosen] = createSignal<string[] | null>(null);
  const chosenNames = () => chosen() ?? defaultColumns(type()?.fields ?? []);
  const columns = () => {
    const all = first()?.fields ?? [];
    return hasFields() ? all : all.filter((c) => chosenNames().includes(c.name));
  };
  const [chooser, setChooser] = createSignal(false);

  const run = (q = draft()) => {
    const next = withType(q, typeName());
    setDraft(next);
    if (next === query()) void load();
    else setQuery(next);
  };
  const sortBy = (field: string) => run(toggleSort(draft(), field));

  // The record dialog: `?id=` opens a record; "new" a new one.
  const [creating, setCreating] = createSignal(false);
  const [open] = createResource(
    () => (props.search.id ? { p: props.project, id: props.search.id } : undefined),
    (k) => api.db.record(k.p, k.id).catch(() => null),
  );
  // A resource keeps its last value when its source goes: read it only while `?id=` is set.
  const openRecord = () => (props.search.id ? (open() ?? undefined) : undefined);
  const closeRecord = () => props.onSearch({ id: undefined });
  const [schema, setSchema] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);

  // Live: a change of this type reloads the page and the type.
  let timer: ReturnType<typeof setTimeout> | undefined;
  const off = events.on((e) => {
    if (e.project !== props.project || (e.kind !== "db.record" && e.kind !== "db.type")) return;
    if (e.data.type !== typeName()) return;
    clearTimeout(timer);
    timer = setTimeout(() => {
      void load();
      if (e.kind === "db.type") {
        if (e.data.change === "deleted") props.onBack();
        else void refetchType();
      }
    }, 300);
  });
  onCleanup(() => {
    token++;
    off();
    clearTimeout(timer);
  });

  const afterSave = () => {
    setCreating(false);
    closeRecord();
    void load();
    void refetchType();
  };

  const removeType = async () => {
    const t = type();
    if (!t) return;
    try {
      await api.db.removeType(props.project, t.name, t.records > 0 ? "records" : "true");
      props.onBack();
    } catch (e) {
      setError(e);
    }
  };

  const qe = () => queryErrorOf(queryError());

  return (
    <>
      <PageHead
        title={
          <span>
            <A href={`${projectBase(props.project)}/knowledge/db`}>DB</A> / <span class="mono">{typeName()}</span>
          </span>
        }
        level={2}
      >
        <button class="primary" disabled={!type()} onClick={() => setCreating(true)}>
          New record
        </button>
        <button disabled={!type()} onClick={() => setSchema(true)}>
          Schema
        </button>
        <Show when={type()}>{(t) => <ConfirmButton label="Delete type" confirmLabel={t().records ? `Yes, delete it and ${t().records} records` : undefined} onConfirm={removeType} />}</Show>
      </PageHead>
      <ErrorBanner error={typeRes.error ?? error()} onDismiss={() => setError(null)} />
      <Show when={type()?.description}>
        <p class="muted small" style={{ margin: "0 0 8px" }}>
          {type()!.description}
        </p>
      </Show>

      <form
        class="db-query"
        onSubmit={(e) => {
          e.preventDefault();
          run();
        }}
      >
        <input
          class="mono"
          type="search"
          aria-label="Query"
          spellcheck={false}
          autocomplete="off"
          value={draft()}
          onInput={(e) => setDraft(e.currentTarget.value)}
          placeholder={`type:${typeName()} status=open sort:-created`}
        />
        <button type="submit">Run</button>
      </form>
      <Show when={qe()} fallback={<ErrorBanner error={queryError()} onDismiss={() => setQueryError(null)} />}>
        {(x) => {
          const parts = () => errorAt(query(), x().position);
          return (
            <div class="db-query-error" role="alert">
              <pre class="mono">
                {parts().before}
                <mark class="db-caret">{parts().at}</mark>
                {parts().after}
              </pre>
              <span class="small">
                Position {x().position}: {x().message}
              </span>
            </div>
          );
        }}
      </Show>
      <p class="muted small db-help">
        Terms: <span class="mono">field=value</span>, <span class="mono">!=</span>, <span class="mono">&gt;</span>, <span class="mono">&lt;=</span>, <span class="mono">field~text</span>, <span class="mono">field:null</span>, <span class="mono">sort:-field</span>,{" "}
        <span class="mono">limit:N</span>, <span class="mono">fields:a,b</span>, <span class="mono">group:field</span>, <span class="mono">count:true</span>. Quote a value with spaces: <span class="mono">title~"login page"</span>.
      </p>

      <Show when={first()}>
        {(r) => (
          <>
            <div class="spread db-bar">
              <span class="muted small">
                {r().count} {r().count === 1 ? "record" : "records"}
                <Show when={r().view === "table" || r().view === "list"}> · {records().length} shown</Show>
              </span>
              <Show when={!hasFields() && !r().groups}>
                <button class="small" aria-expanded={chooser()} onClick={() => setChooser((x) => !x)}>
                  Columns
                </button>
              </Show>
            </div>
            <Show when={chooser() && !hasFields()}>
              <div class="db-chooser card-box">
                <For each={type()?.fields ?? []}>
                  {(f) => (
                    <label class="db-check">
                      <input
                        type="checkbox"
                        checked={chosenNames().includes(f.name)}
                        onChange={(e) => {
                          const on = e.currentTarget.checked;
                          const all = (type()?.fields ?? []).map((x) => x.name);
                          const now = new Set(chosenNames());
                          if (on) now.add(f.name);
                          else now.delete(f.name);
                          setChosen(all.filter((n) => now.has(n)));
                        }}
                      />{" "}
                      {f.name}
                    </label>
                  )}
                </For>
              </div>
            </Show>
            <Show
              when={r().groups}
              fallback={
                <Show when={r().view !== "count"} fallback={<div class="db-count">{r().count}</div>}>
                  <Show when={records().length} fallback={<Empty>No records match.</Empty>}>
                    <div class="table-wrap card-box db-table-box">
                      <table class="db-table">
                        <thead>
                          <tr>
                            <For each={columns()}>
                              {(c) => {
                                const sortable = () => !["list", "json", "text"].includes(c.kind);
                                const dir = () => sortOf(query(), c.name);
                                return (
                                  <th aria-sort={dir() === "asc" ? "ascending" : dir() === "desc" ? "descending" : undefined}>
                                    <Show when={sortable()} fallback={c.name}>
                                      <button type="button" class="link-button db-sort" title={`Sort by ${c.name}`} onClick={() => sortBy(c.name)}>
                                        {c.name}
                                        <span class="db-sort-mark" aria-hidden="true">
                                          {dir() === "asc" ? " ▲" : dir() === "desc" ? " ▼" : ""}
                                        </span>
                                      </button>
                                    </Show>
                                  </th>
                                );
                              }}
                            </For>
                          </tr>
                        </thead>
                        <tbody>
                          <For each={records().map((record) => record.id)}>
                            {(id) => {
                              const rec = (): DbRecord => recordsById().get(id)!;
                              return (
                              <tr class="clickable" tabIndex={0} onClick={() => props.onSearch({ id })} onKeyDown={(e) => e.key === "Enter" && props.onSearch({ id })}>
                                <For each={columns()}>{(c) => <td class={`db-cell db-cell-${c.kind}`}>{cellText(c, columnValue(c, rec()), labels())}</td>}</For>
                              </tr>
                              );
                            }}
                          </For>
                        </tbody>
                      </table>
                    </div>
                  </Show>
                </Show>
              }
            >
              {(groups) => (
                <div class="table-wrap card-box db-table-box">
                  <table class="db-table db-groups">
                    <thead>
                      <tr>
                        <th>{r().group}</th>
                        <th class="num">Count</th>
                      </tr>
                    </thead>
                    <tbody>
                      <For each={groups()}>
                        {(g) => (
                          <tr>
                            <td>{g.value === null ? "(none)" : typeof g.value === "string" ? (labels()[g.value] ?? g.value) : String(g.value)}</td>
                            <td class="num">{g.count}</td>
                          </tr>
                        )}
                      </For>
                    </tbody>
                  </table>
                </div>
              )}
            </Show>
            <Show when={pages().at(-1)?.next_cursor}>
              <div class="row load-more">
                <button class="small" disabled={loading()} onClick={() => void load(true)}>
                  {loading() ? "Loading…" : "Load more"}
                </button>
              </div>
            </Show>
          </>
        )}
      </Show>

      {/* Keyed: another record makes a new dialog with its own form. */}
      <Show when={type() ? (creating() ? "new" : openRecord()?.id) : undefined} keyed>
        {(k) => <RecordModal project={props.project} type={type()!} record={k === "new" ? undefined : openRecord()} onClose={() => (setCreating(false), closeRecord())} onSaved={afterSave} onDeleted={afterSave} />}
      </Show>
      <Show when={schema() && type()}>
        <SchemaEditor
          project={props.project}
          type={type()!}
          typeNames={(types() ?? []).map((t) => t.name)}
          onClose={() => setSchema(false)}
          onSaved={(t) => {
            setSchema(false);
            setType(t);
            setChosen(null);
            void refetchTypes();
            void load();
          }}
        />
      </Show>
      <Show when={!type() && !typeRes.loading && !typeRes.error}>
        <Empty>
          No type <span class="mono">{typeName()}</span>. <A href={`${projectBase(props.project)}/knowledge/db`}>All types</A>
        </Empty>
      </Show>
    </>
  );
}
