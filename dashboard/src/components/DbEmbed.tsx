import { InstanceAnchor } from "../instance/navigation";
import { useInstance } from "../instance";
// The answer of a wiki `db` block (docs/api.md §27). lib/markdown turns the
// fence into `<div class="db-embed" data-db-query>`; components/Markdown
// mounts this component in it, with the query read as text. It runs the
// query in the project of the page, with the rights of the viewer, and
// shows the answer as text nodes only: a table, a list, a count, or the
// counts of a group. A ref cell links to the record in the DB tab. An
// error shows as a muted line.

import { createResource, For, Match, onCleanup, Show, Switch } from "solid-js";
import type { DbQueryResult } from "../api/types";
import { cellText, columnValue, groupText, typeHref } from "../features/db/model";
import { describeError } from "../lib/errors";


export function DbEmbed(props: { project: string; query: string }) {
  const { api, events } = useInstance();
  const [result, { refetch }] = createResource(
    () => ({ p: props.project, q: props.query }),
    (k) => api.db.query(k.p, k.q),
  );
  // A resource in error throws when read: read it only without an error.
  const answer = () => (result.error ? undefined : result());
  // Live: a record or a type of the project changed.
  let timer: ReturnType<typeof setTimeout> | undefined;
  const off = events.on((e) => {
    if ((e.kind === "db.record" || e.kind === "db.type") && e.project === props.project) {
      clearTimeout(timer);
      timer = setTimeout(() => void refetch(), 500);
    }
  });
  onCleanup(() => {
    off();
    clearTimeout(timer);
  });

  return (
    <div class="db-embed-box">
      <div class="db-embed-head small">
        <code class="db-embed-query">{props.query}</code>
        <Show when={answer()}>{(r) => <InstanceAnchor href={typeHref(props.project, r().type, { q: props.query })}>Open in DB</InstanceAnchor>}</Show>
      </div>
      <Show when={!result.error} fallback={<span class="muted small db-embed-error">DB query failed: {describeError(result.error)?.message ?? "unknown error"}</span>}>
        <Show when={answer()} fallback={<span class="muted small">Loading…</span>}>
          {(r) => <DbAnswer project={props.project} r={r()} />}
        </Show>
      </Show>
    </div>
  );
}

function DbAnswer(props: { project: string; r: DbQueryResult }) {
  const cols = () => props.r.fields;
  /** A cell: a ref links to its record; any other value is text. */
  const Cell = (p: { col: DbQueryResult["fields"][number]; rec: DbQueryResult["records"][number] }) => {
    const v = () => columnValue(p.col, p.rec);
    return (
      <Show when={p.col.kind === "ref" && typeof v() === "string" && v()} fallback={cellText(p.col, v(), props.r.labels)}>
        {(id) => <InstanceAnchor href={typeHref(props.project, p.col.ref ?? "", { id: id() as string })}>{cellText(p.col, id(), props.r.labels)}</InstanceAnchor>}
      </Show>
    );
  };
  return (
    <Switch>
      <Match when={props.r.view === "count"}>
        <div class="db-count">{props.r.count}</div>
      </Match>
      <Match when={props.r.groups}>
        {(groups) => (
          <div class="table-wrap">
            <table class="db-table db-groups">
              <thead>
                <tr>
                  <th>{props.r.group}</th>
                  <th class="num">Count</th>
                </tr>
              </thead>
              <tbody>
                <For each={groups()}>
                  {(g) => (
                    <tr>
                      <td>{groupText(props.r, g.value)}</td>
                      <td class="num">{g.count}</td>
                    </tr>
                  )}
                </For>
              </tbody>
            </table>
          </div>
        )}
      </Match>
      <Match when={!props.r.records.length}>
        <span class="muted small">No records match.</span>
      </Match>
      <Match when={props.r.view === "list"}>
        <ul class="db-list">
          <For each={props.r.records}>
            {(rec) => (
              <li>
                <InstanceAnchor href={typeHref(props.project, props.r.type, { id: rec.id })}>{cellText(cols()[0] ?? { name: "id", kind: "string", required: false, meta: true }, cols()[0] ? columnValue(cols()[0]!, rec) : rec.id, props.r.labels) || rec.id}</InstanceAnchor>
                <Show when={cols().length > 1}>
                  <span class="muted">
                    {" "}
                    · {cols()
                      .slice(1)
                      .map((c) => cellText(c, columnValue(c, rec), props.r.labels))
                      .filter(Boolean)
                      .join(" · ")}
                  </span>
                </Show>
              </li>
            )}
          </For>
        </ul>
      </Match>
      <Match when={true}>
        <div class="table-wrap">
          <table class="db-table">
            <thead>
              <tr>
                <For each={cols()}>{(c) => <th>{c.name}</th>}</For>
              </tr>
            </thead>
            <tbody>
              <For each={props.r.records}>
                {(rec) => (
                  <tr>
                    <For each={cols()}>
                      {(c) => (
                        <td class={`db-cell db-cell-${c.kind}`}>
                          <Cell col={c} rec={rec} />
                        </td>
                      )}
                    </For>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </div>
        <Show when={props.r.count > props.r.records.length}>
          <div class="muted small">
            {props.r.records.length} of {props.r.count} records.
          </div>
        </Show>
      </Match>
    </Switch>
  );
}
