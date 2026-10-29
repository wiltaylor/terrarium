// The Audit tab of the Logs screen: who did what, newest first, with
// filters by actor, action, and project. New rows come in on top from the
// `audit` frames of the event stream (docs/api.md §9.1), in the live mode
// of the log tabs (components/Live.tsx).

import { createEffect, createSignal, For, on, onCleanup, Show } from "solid-js";
import { createLive, createScrollKeeper, LiveControl, NewAbove } from "../../components/Live";
import { createPagedList } from "../../components/paged";
import { Empty, ErrorBanner, LoadMore, PageHead } from "../../components/ui";
import type { AuditEntry, AuditQuery } from "../../api/types";
import { ago } from "../../lib/format";
import { agents, api, events, projects } from "../../stores";

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

/** True when a live row passes the filters of the tab (the daemon does the same for a page). */
export const auditMatches = (e: AuditEntry, q: Pick<AuditQuery, "actor" | "action" | "project">): boolean =>
  (!q.actor || e.actor === q.actor) && (!q.action || e.action === q.action) && (!q.project || e.project === q.project);

export default function AuditPage() {
  const [actor, setActor] = createSignal("");
  const [action, setAction] = createSignal("");
  const [project, setProject] = createSignal("");
  const query = () => ({ actor: actor() || undefined, action: action() || undefined, project: project() || undefined });
  const key = () => `${actor()}|${action()}|${project()}`;
  const list = createPagedList(key, (_k, cursor) => api.audit({ ...query(), order: "desc", cursor }), (e) => e.id);
  const live = createLive();
  // A new row goes on top (held while off); a changed row (a new count) changes in place.
  const off = events.onAudit((e) => {
    if (list.initial() || !auditMatches(e, query())) return;
    const known = list.items().some((x) => x.id === e.id);
    if (!known) live.arrive([e.id]);
    list.upsert([e], "start");
  });
  onCleanup(off);
  // A new list loads every row again: nothing stays held.
  createEffect(on(key, () => live.release(), { defer: true }));
  const keeper = createScrollKeeper("tr");
  const shown = () => live.shown(list.items(), (e) => e.id);
  const actors = () => [...new Set(["operator", "system", ...list.items().map((e) => e.actor)])].sort();
  return (
    <>
      <PageHead title="Audit" level={2}>
        <LiveControl live={live} count={live.state().held.length} />
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
        <button onClick={() => (live.release(), void list.reload())}>Refresh</button>
      </PageHead>
      <ErrorBanner error={list.error()} />
      <NewAbove live={live} keeper={keeper} held={live.state().held.length} />
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
          <tbody ref={keeper.ref}>
            <For each={shown()}>
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
        <LoadMore hasMore={list.hasMore()} loading={list.loading()} onMore={list.loadMore} count={shown().length} />
      </div>
    </>
  );
}
