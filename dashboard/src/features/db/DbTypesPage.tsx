import { useInstance } from "../../instance";
// The DB tab of the knowledge screen (app/nav.ts KNOWLEDGE_TABS,
// /p/:project/knowledge/db): the record types of the project, with their
// field and record counts. "New type" opens the schema editor. A type
// opens its records (/p/:project/knowledge/db/:type).

import { useParams } from "@solidjs/router";
import { A, useNavigate } from "../../instance/navigation";
import { createResource, createSignal, For, onCleanup, Show } from "solid-js";
import { Empty, ErrorBanner, PageHead } from "../../components/ui";
import { ago } from "../../lib/format";

import { typeHref } from "./model";
import { SchemaEditor } from "./SchemaEditor";

export default function DbTypesPage() {
  const params = useParams<{ project: string }>();
  return <DbTypesPanel project={params.project} />;
}

export function DbTypesPanel(props: { project: string }) {
  return <Show when={props.project} keyed>{(project) => <DbTypesContent project={project} />}</Show>;
}

function DbTypesContent(props: { project: string }) {
  const { api, events } = useInstance();
  const navigate = useNavigate();
  const [typesRes, { refetch }] = createResource(() => props.project, (p) => api.db.types(p));
  // A resource in error throws when read: read it only without an error.
  const types = () => (typesRes.error ? undefined : typesRes());
  const [creating, setCreating] = createSignal(false);
  let timer: ReturnType<typeof setTimeout> | undefined;
  // Live: a type or a record of this project changed.
  const off = events.on((e) => {
    if ((e.kind === "db.type" || e.kind === "db.record") && e.project === props.project) {
      clearTimeout(timer);
      timer = setTimeout(() => void refetch(), 300);
    }
  });
  onCleanup(() => {
    off();
    clearTimeout(timer);
  });

  return (
    <>
      <PageHead title="DB" level={2}>
        <button class="primary" onClick={() => setCreating(true)}>
          New type
        </button>
      </PageHead>
      <ErrorBanner error={typesRes.error} />
      <p class="muted small" style={{ margin: "0 0 10px" }}>
        Structured data of the project: record types with a schema. Agents use <span class="mono">terrarium db</span>; a wiki page shows a query with a <span class="mono">```db</span> block.
      </p>
      <Show when={types()} fallback={<Show when={!typesRes.error}><Empty>Loading…</Empty></Show>}>
        <Show when={types()!.length} fallback={<Empty>No types yet. Make one with "New type", or ask an agent to.</Empty>}>
          <div class="table-wrap card-box db-table-box">
            <table class="db-table">
              <thead>
                <tr>
                  <th>Type</th>
                  <th>Description</th>
                  <th class="num">Fields</th>
                  <th class="num">Records</th>
                  <th>Updated</th>
                </tr>
              </thead>
              <tbody>
                <For each={types()}>
                  {(t) => (
                    <tr class="clickable" onClick={() => navigate(typeHref(props.project, t.name))}>
                      <td>
                        <A href={typeHref(props.project, t.name)} class="mono" onClick={(e) => e.stopPropagation()}>
                          {t.name}
                        </A>
                      </td>
                      <td class="db-desc-cell">{t.description}</td>
                      <td class="num">{t.fields.length}</td>
                      <td class="num">{t.records}</td>
                      <td class="muted small">{ago(t.updated_at)}</td>
                    </tr>
                  )}
                </For>
              </tbody>
            </table>
          </div>
        </Show>
      </Show>
      <Show when={creating()}>
        <SchemaEditor
          project={props.project}
          typeNames={(types() ?? []).map((t) => t.name)}
          onClose={() => setCreating(false)}
          onSaved={(t) => {
            setCreating(false);
            void refetch();
            navigate(typeHref(props.project, t.name));
          }}
        />
      </Show>
    </>
  );
}
