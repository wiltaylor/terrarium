import { createSignal, For, Show } from "solid-js";
import { createPagedList } from "../../components/paged";
import { Empty, ErrorBanner, LoadMore, PageHead } from "../../components/ui";
import type { AuditEntry } from "../../api/types";
import { ago } from "../../lib/format";
import { agents, api, projects } from "../../stores";

/** `agent:<ulid>` reads better as `project/name`. */
export function actorName(actor: string): string {
  if (!actor.startsWith("agent:")) return actor;
  const a = agents.byId(actor.slice(6));
  return a ? `${a.project}/${a.name}` : actor;
}

/** An audit row's `detail` object as short `key: value` chips. */
export function AuditDetail(props: { detail: AuditEntry["detail"] }) {
  const entries = () => Object.entries(props.detail ?? {}).filter(([, v]) => v !== null && v !== undefined);
  return (
    <Show when={entries().length} fallback={<span class="muted">—</span>}>
      <span class="row" style={{ "flex-wrap": "wrap", gap: "4px" }}>
        <For each={entries()}>
          {([k, v]) => (
            <span class="chip mono small" title={JSON.stringify(v)}>
              {k}: {typeof v === "string" ? v : JSON.stringify(v)}
            </span>
          )}
        </For>
      </span>
    </Show>
  );
}

const ACTIONS = ["project.create", "project.update", "project.delete", "project.detach", "repo.attach", "agent.create", "agent.update", "agent.delete", "agent.roles", "agent.start", "agent.stop", "agent.restart", "agent.send", "agent.answer", "agent.attach", "board.columns", "card.reassign", "card.reopen", "card.archive", "definitions.sync"];

export default function AuditPage() {
  const [actor, setActor] = createSignal("");
  const [action, setAction] = createSignal("");
  const [project, setProject] = createSignal("");
  const list = createPagedList(
    () => `${actor()}|${action()}|${project()}`,
    (_k, cursor) => api.audit({ actor: actor() || undefined, action: action() || undefined, project: project() || undefined, cursor }),
    (e) => e.id,
  );
  const actors = () => [...new Set(["operator", "system", ...list.items().map((e) => e.actor)])].sort();
  return (
    <>
      <PageHead title="Audit">
        <select value={actor()} onChange={(e) => setActor(e.currentTarget.value)} aria-label="Actor">
          <option value="">Any actor</option>
          <For each={actors()}>{(a) => <option value={a}>{actorName(a)}</option>}</For>
        </select>
        <select value={action()} onChange={(e) => setAction(e.currentTarget.value)} aria-label="Action">
          <option value="">Any action</option>
          <For each={ACTIONS}>{(a) => <option value={a}>{a}</option>}</For>
        </select>
        <select value={project()} onChange={(e) => setProject(e.currentTarget.value)} aria-label="Project">
          <option value="">Any project</option>
          <For each={projects.projects()}>{(p) => <option value={p.name}>{p.name}</option>}</For>
        </select>
        <button onClick={() => list.reload()}>Refresh</button>
      </PageHead>
      <ErrorBanner error={list.error()} />
      <div class="card-box table-wrap">
        <table>
          <thead>
            <tr>
              <th>When</th>
              <th>Actor</th>
              <th>Action</th>
              <th>Target</th>
              <th>Project</th>
              <th>Detail</th>
            </tr>
          </thead>
          <tbody>
            <For each={list.items()}>
              {(e) => (
                <tr>
                  <td title={e.at}>{ago(e.at)}</td>
                  <td>{actorName(e.actor)}</td>
                  <td class="mono small">{e.action}</td>
                  <td class="mono small">{agents.byId(e.target) ? `${agents.byId(e.target)!.project}/${agents.byId(e.target)!.name}` : e.target}</td>
                  <td>{e.project ?? ""}</td>
                  <td>
                    <AuditDetail detail={e.detail} />
                  </td>
                </tr>
              )}
            </For>
          </tbody>
        </table>
        <Show when={!list.initial() && !list.items().length}>
          <Empty>No audit rows match.</Empty>
        </Show>
        <LoadMore hasMore={list.hasMore()} loading={list.loading()} onMore={list.loadMore} count={list.items().length} />
      </div>
    </>
  );
}
