import { InstanceAnchor } from "../../instance/navigation";
import { useInstance } from "../../instance";
// The Memory tab of the knowledge screen (app/nav.ts KNOWLEDGE_TABS): the
// memories of the project, and the graph of the memory of `?id=`.

import { useParams, useSearchParams } from "@solidjs/router";
import { A } from "../../instance/navigation";
import { createMemo, createResource, createSignal, For, Show } from "solid-js";
import { Empty, PageHead } from "../../components/ui";
import { RELATIONS, type Memory, type MemoryEdge, type Relation } from "../../api/types";
import { ago, humanize } from "../../lib/format";

import { MemoryGraph } from "./MemoryGraph";

const FIRST: Relation[] = ["contradicts", "supersedes"];

export function sortLinks(links: MemoryEdge[]): MemoryEdge[] {
  return links.slice().sort((a, b) => Number(FIRST.includes(b.relation)) - Number(FIRST.includes(a.relation)));
}

export default function MemoryPage() {
  const { api } = useInstance();
  const params = useParams<{ project: string }>();
  const [search, setSearch] = useSearchParams<{ id?: string }>();
  const [q, setQ] = createSignal("");
  const [tag, setTag] = createSignal("");
  const [author, setAuthor] = createSignal("");
  const [relation, setRelation] = createSignal<Relation | "">("");
  const [all, setAll] = createSignal(false);

  const [list] = createResource(
    () => ({ project: params.project, q: q(), tag: tag(), author: author(), all: all() }),
    (k) => api.memories.list(k.project, { q: k.q || undefined, tag: k.tag || undefined, author: k.author || undefined, all: k.all }),
  );
  const [everything] = createResource(() => params.project, (p) => api.memories.list(p, { all: true }));
  const tags = () => [...new Set((everything() ?? []).flatMap((m) => m.tags))].sort();
  const authors = () => [...new Map((everything() ?? []).map((m) => [m.author, m.author_name ?? m.author])).entries()].sort((a, b) => a[1].localeCompare(b[1]));
  const recent = () => (everything() ?? []).slice().sort((a, b) => b.updated_at.localeCompare(a.updated_at)).slice(0, 6);

  const selectedId = () => search.id ?? list()?.[0]?.id;
  // `project/MEM-n`: a bare ID is ambiguous for the operator (docs/api.md §10.1).
  const [graph] = createResource(selectedId, (id) => api.memories.graph(`${params.project}/${id}`, 1));
  const selected = createMemo<Memory | undefined>(() => graph()?.nodes.find((n) => n.id === selectedId()));
  const filteredGraph = () => {
    const g = graph();
    if (!g || !relation()) return g;
    return { ...g, edges: g.edges.filter((l) => l.relation === relation()) };
  };
  const sourceHref = (s: Memory["sources"][number]) =>
    s.kind === "card" ? `/p/${params.project}/board?card=${s.ref}` : s.kind === "thread" ? `/logs/mail` : s.kind === "url" && /^https?:/.test(s.ref) ? s.ref : undefined;

  return (
    <>
      <PageHead title="Memory" level={2}>
        <input type="search" placeholder="Search memories" value={q()} onInput={(e) => setQ(e.currentTarget.value)} aria-label="Search" />
        <select value={tag()} onChange={(e) => setTag(e.currentTarget.value)} aria-label="Tag">
          <option value="">Any tag</option>
          <For each={tags()}>{(t) => <option value={t}>{t}</option>}</For>
        </select>
        <select value={author()} onChange={(e) => setAuthor(e.currentTarget.value)} aria-label="Author">
          <option value="">Any author</option>
          <For each={authors()}>{([addr, name]) => <option value={addr}>{name}</option>}</For>
        </select>
        <select value={relation()} onChange={(e) => setRelation(e.currentTarget.value as Relation | "")} aria-label="Relation">
          <option value="">Any relation</option>
          <For each={RELATIONS}>{(r) => <option value={r}>{humanize(r)}</option>}</For>
        </select>
        <label class="row" style={{ display: "flex" }}>
          <input type="checkbox" checked={all()} onChange={(e) => setAll(e.currentTarget.checked)} /> superseded
        </label>
      </PageHead>

      <div class="split">
        <div class="stack">
          <div class="card-box thread-list" style={{ padding: "4px" }}>
            <For each={list()} fallback={<Empty>No memories match.</Empty>}>
              {(m) => (
                <button class={selectedId() === m.id ? "active" : ""} onClick={() => setSearch({ id: m.id })}>
                  <span class="subject">
                    <span class="mono muted small">{m.id}</span> {m.title}
                  </span>
                  <span class="muted small">
                    {m.author_name ?? m.author} · {m.tags.join(", ")}
                    {m.superseded_by ? ` · superseded by ${m.superseded_by}` : ""}
                  </span>
                </button>
              )}
            </For>
          </div>
          <div class="card-box">
            <h3>Recent writes</h3>
            <ul class="timeline small">
              <For each={recent()}>
                {(m) => (
                  <li>
                    <InstanceAnchor href={`?id=${m.id}`} onClick={(e) => (e.preventDefault(), setSearch({ id: m.id }))}>
                      {m.id}
                    </InstanceAnchor{" "}
                    {m.title} <span class="muted">· {m.author_name ?? m.author}, {ago(m.updated_at)}</span>
                  </li>
                )}
              </For>
            </ul>
          </div>
        </div>

        <div class="stack">
          <Show when={filteredGraph()}>{(g) => <MemoryGraph graph={g()} center={selectedId()!} onPick={(id) => setSearch({ id })} />}</Show>
          <Show when={selected()}>
            {(m) => (
              <div class="card-box stack" style={{ gap: "8px" }}>
                <div class="spread">
                  <h2 style={{ margin: 0 }}>
                    <span class="mono muted">{m().id}</span> {m().title}
                  </h2>
                  <Show when={m().superseded_by}>
                    <span class="chip warn">superseded by {m().superseded_by}</span>
                  </Show>
                </div>
                <div class="muted small">
                  {m().author_name ?? m().author} · updated {ago(m().updated_at)} · {m().tags.map((t) => `#${t}`).join(" ")}
                </div>
                <p style={{ margin: 0, "white-space": "pre-wrap" }}>{m().body}</p>
                <Show when={m().sources.length}>
                  <div class="small">
                    Sources:{" "}
                    <For each={m().sources}>
                      {(s) => (
                        <Show when={sourceHref(s)} fallback={<code>{`${s.kind}:${s.ref}`} </code>}>
                          {(href) => <A href={href()}>{`${s.kind}:${s.ref}`} </A>}
                        </Show>
                      )}
                    </For>
                  </div>
                </Show>
                <h3 style={{ margin: "6px 0 0" }}>Links</h3>
                <ul class="small" style={{ margin: 0 }}>
                  <For each={sortLinks(m().links)} fallback={<li class="muted">No links.</li>}>
                    {(l) => (
                      <li>
                        <span class={`chip ${FIRST.includes(l.relation) ? "bad" : ""}`}>{humanize(l.relation)}</span>{" "}
                        {l.from === m().id ? "→" : "←"}{" "}
                        <InstanceAnchor href={`?id=${l.from === m().id ? l.to : l.from}`} onClick={(e) => (e.preventDefault(), setSearch({ id: l.from === m().id ? l.to : l.from }))}>
                          {l.from === m().id ? l.to : l.from}
                        </InstanceAnchor{" "}
                        <Show when={l.other_title}>
                          <span>{l.other_title} </span>
                        </Show>
                        <span class="muted">by {l.author_name ?? l.author}</span>
                      </li>
                    )}
                  </For>
                </ul>
              </div>
            )}
          </Show>
        </div>
      </div>
    </>
  );
}
