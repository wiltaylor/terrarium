import { createResource, createSignal, For } from "solid-js";
import { PageHead } from "../../components/ui";
import { ago } from "../../lib/format";
import { api } from "../../stores";

export default function AuditPage() {
  const [actor, setActor] = createSignal("");
  const [entries] = createResource(actor, (a) => api.audit({ actor: a || undefined }));
  const [all] = createResource(() => api.audit());
  const actors = () => [...new Set((all() ?? []).map((e) => e.actor))].sort();
  return (
    <>
      <PageHead title="Audit">
        <select value={actor()} onChange={(e) => setActor(e.currentTarget.value)} aria-label="Actor">
          <option value="">Any actor</option>
          <For each={actors()}>{(a) => <option value={a}>{a}</option>}</For>
        </select>
      </PageHead>
      <div class="card-box table-wrap">
        <table>
          <thead>
            <tr>
              <th>When</th>
              <th>Actor</th>
              <th>Action</th>
              <th>Target</th>
              <th>Detail</th>
            </tr>
          </thead>
          <tbody>
            <For each={entries()}>
              {(e) => (
                <tr>
                  <td title={e.at}>{ago(e.at)}</td>
                  <td>{e.actor}</td>
                  <td class="mono small">{e.action}</td>
                  <td>{e.target}</td>
                  <td class="muted">{e.detail ?? ""}</td>
                </tr>
              )}
            </For>
          </tbody>
        </table>
      </div>
    </>
  );
}
