import { useParams, useSearchParams } from "@solidjs/router";
import { createEffect, createResource, createSignal, For, on, onCleanup, Show } from "solid-js";
import { StatusBadge } from "../../components/StatusBadge";
import { Drawer, ErrorBanner, Modal, PageHead } from "../../components/ui";
import type { Card } from "../../api/types";
import { ago, humanize } from "../../lib/format";
import { cardsIn, createBoardStore } from "../../stores/board";
import { agents, api, events } from "../../stores";

type Drop = { column: string; index: number };

export function BoardPage() {
  const params = useParams<{ project: string }>();
  const [search, setSearch] = useSearchParams<{ card?: string }>();
  const board = createBoardStore(api);
  const [dragging, setDragging] = createSignal<string | null>(null);
  const [drop, setDrop] = createSignal<Drop | null>(null);
  const [pendingBlock, setPendingBlock] = createSignal<{ id: string; index: number } | null>(null);
  const [label, setLabel] = createSignal("");
  const [assignee, setAssignee] = createSignal("");
  const [newTitle, setNewTitle] = createSignal("");

  createEffect(on(() => params.project, (p) => void board.load(p)));

  let reloadTimer: ReturnType<typeof setTimeout> | undefined;
  const off = events.on((e) => {
    if (e.kind === "card" && e.project === params.project && !dragging()) {
      clearTimeout(reloadTimer);
      reloadTimer = setTimeout(() => void board.reload(), 300);
    }
  });
  onCleanup(() => {
    off();
    clearTimeout(reloadTimer);
  });

  const labels = () => [...new Set(board.state.board?.cards.flatMap((c) => c.labels) ?? [])].sort();
  const assignees = () => [...new Set(board.state.board?.cards.map((c) => c.assignee).filter((a): a is string => !!a) ?? [])].sort();
  const visible = (c: Card) => (!label() || c.labels.includes(label())) && (!assignee() || c.assignee === assignee());

  const dropIndex = (columnEl: HTMLElement, y: number) => {
    const cards = [...columnEl.querySelectorAll<HTMLElement>("[data-card]")].filter((el) => el.dataset.card !== dragging());
    const i = cards.findIndex((el) => {
      const r = el.getBoundingClientRect();
      return y < r.top + r.height / 2;
    });
    return i < 0 ? cards.length : i;
  };

  const commitMove = async (id: string, column: string, index: number, comment?: string) => {
    if (column === "blocked" && !comment) {
      setPendingBlock({ id, index });
      return;
    }
    await board.move(id, column, index, comment);
  };

  const selected = () => board.state.board?.cards.find((c) => c.id === search.card);

  return (
    <>
      <PageHead title={`Board · ${params.project}`}>
        <select value={label()} onChange={(e) => setLabel(e.currentTarget.value)} aria-label="Filter by label">
          <option value="">Any label</option>
          <For each={labels()}>{(l) => <option value={l}>{l}</option>}</For>
        </select>
        <select value={assignee()} onChange={(e) => setAssignee(e.currentTarget.value)} aria-label="Filter by assignee">
          <option value="">Any assignee</option>
          <For each={assignees()}>{(a) => <option value={a}>{a}</option>}</For>
        </select>
        <form
          class="row"
          onSubmit={async (e) => {
            e.preventDefault();
            if (newTitle().trim() && (await board.create(newTitle().trim()))) setNewTitle("");
          }}
        >
          <input placeholder="New card title" value={newTitle()} onInput={(e) => setNewTitle(e.currentTarget.value)} aria-label="New card title" />
          <button class="primary" type="submit">
            Add card
          </button>
        </form>
      </PageHead>
      <ErrorBanner error={board.state.error} onDismiss={board.clearError} />

      <Show when={board.state.board} fallback={<p class="muted">{board.state.loading ? "Loading…" : ""}</p>}>
        {(b) => (
          <div class="board">
            <For each={b().columns}>
              {(col) => {
                const cards = () => cardsIn(b(), col.name);
                const over = () => col.wip_limit !== undefined && cards().length > col.wip_limit;
                const othersIndex = (id: string) => cards().filter((c) => c.id !== dragging()).findIndex((c) => c.id === id);
                return (
                  <section
                    class={`column${drop()?.column === col.name ? " drop" : ""}${over() ? " over-limit" : ""}`}
                    aria-label={col.name}
                    onDragOver={(e) => {
                      if (!dragging()) return;
                      e.preventDefault();
                      e.dataTransfer!.dropEffect = "move";
                      const index = dropIndex(e.currentTarget, e.clientY);
                      const d = drop();
                      if (!d || d.column !== col.name || d.index !== index) setDrop({ column: col.name, index });
                    }}
                    onDragLeave={(e) => {
                      if (!e.currentTarget.contains(e.relatedTarget as Node)) setDrop(null);
                    }}
                    onDrop={(e) => {
                      e.preventDefault();
                      const id = e.dataTransfer?.getData("text/plain") || dragging();
                      const d = drop();
                      setDrop(null);
                      setDragging(null);
                      if (id && d) void commitMove(id, d.column, d.index);
                    }}
                  >
                    <div class="column-head">
                      <span>{humanize(col.name)}</span>
                      <span class="count muted small">
                        {cards().length}
                        {col.wip_limit !== undefined ? ` / ${col.wip_limit}` : ""}
                      </span>
                    </div>
                    <For each={cards()}>
                      {(card) => (
                        <>
                          <Show when={drop()?.column === col.name && dragging() !== card.id && othersIndex(card.id) === drop()?.index}>
                            <div class="drop-marker" />
                          </Show>
                          <KCard
                            card={card}
                            hidden={!visible(card)}
                            dragging={dragging() === card.id}
                            onOpen={() => setSearch({ card: card.id })}
                            onDragStart={(e) => {
                              e.dataTransfer!.setData("text/plain", card.id);
                              e.dataTransfer!.effectAllowed = "move";
                              setDragging(card.id);
                            }}
                            onDragEnd={() => {
                              setDragging(null);
                              setDrop(null);
                            }}
                          />
                        </>
                      )}
                    </For>
                    <Show when={drop()?.column === col.name && drop()!.index >= cards().filter((c) => c.id !== dragging()).length}>
                      <div class="drop-marker" />
                    </Show>
                  </section>
                );
              }}
            </For>
          </div>
        )}
      </Show>

      <Show when={pendingBlock()}>
        {(p) => (
          <BlockModal
            cardId={p().id}
            onCancel={() => setPendingBlock(null)}
            onSubmit={async (comment) => {
              const { id, index } = p();
              setPendingBlock(null);
              await commitMove(id, "blocked", index, comment);
            }}
          />
        )}
      </Show>

      <Show when={selected()}>
        {(c) => (
          <CardDrawer
            card={c()}
            columns={board.state.board?.columns.map((x) => x.name) ?? []}
            onClose={() => setSearch({ card: undefined })}
            onMove={(column, comment) => {
              const b = board.state.board;
              const end = b ? cardsIn(b, column).filter((x) => x.id !== c().id).length : 0;
              return commitMove(c().id, column, end, comment);
            }}
            onSaved={board.put}
          />
        )}
      </Show>
    </>
  );
}

function KCard(props: {
  card: Card;
  hidden: boolean;
  dragging: boolean;
  onOpen: () => void;
  onDragStart: (e: DragEvent) => void;
  onDragEnd: () => void;
}) {
  const owner = () => (props.card.assignee ? agents.byName(props.card.project, props.card.assignee) : undefined);
  return (
    <article
      class={`kcard${props.dragging ? " dragging" : ""}`}
      style={{ display: props.hidden ? "none" : undefined }}
      data-card={props.card.id}
      draggable={true}
      tabIndex={0}
      onDragStart={(e) => props.onDragStart(e)}
      onDragEnd={() => props.onDragEnd()}
      onClick={() => props.onOpen()}
      onKeyDown={(e) => e.key === "Enter" && props.onOpen()}
    >
      <div class="spread small">
        <span class="mono muted">{props.card.id}</span>
        <Show when={props.card.depends_on.length}>
          <span class="chip warn" title={`Depends on ${props.card.depends_on.join(", ")}`}>
            deps {props.card.depends_on.length}
          </span>
        </Show>
      </div>
      <div class="title">{props.card.title}</div>
      <div class="row small">
        <For each={props.card.labels}>{(l) => <span class="chip">{l}</span>}</For>
      </div>
      <Show when={props.card.assignee}>
        <div class="spread small">
          <span>{props.card.assignee}</span>
          <Show when={owner()}>{(a) => <StatusBadge status={a().status} />}</Show>
        </div>
      </Show>
    </article>
  );
}

function BlockModal(props: { cardId: string; onCancel: () => void; onSubmit: (comment: string) => void }) {
  const [text, setText] = createSignal("");
  return (
    <Modal title={`Block ${props.cardId}`} onClose={() => props.onCancel()}>
      <p class="muted" style={{ margin: 0 }}>
        Moving a card to blocked needs a comment. The daemon mails it to the operator and the card's creator.
      </p>
      <textarea value={text()} onInput={(e) => setText(e.currentTarget.value)} placeholder="What is needed to unblock it?" aria-label="Comment" />
      <div class="row">
        <button class="primary" disabled={!text().trim()} onClick={() => props.onSubmit(text().trim())}>
          Move to blocked
        </button>
        <button onClick={() => props.onCancel()}>Cancel</button>
      </div>
    </Modal>
  );
}

function CardDrawer(props: {
  card: Card;
  columns: string[];
  onClose: () => void;
  onMove: (column: string, comment?: string) => Promise<void>;
  onSaved: (c: Card) => void;
}) {
  const [comment, setComment] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const [editing, setEditing] = createSignal(false);
  const [title, setTitle] = createSignal("");
  const [body, setBody] = createSignal("");
  const [history] = createResource(() => props.card.id, (id) => api.cards.events(id));
  const projectAgents = () => agents.inProject(props.card.project);

  const save = async (patch: Parameters<typeof api.cards.update>[1]) => {
    try {
      props.onSaved(await api.cards.update(props.card.id, patch));
      setError(null);
      return true;
    } catch (e) {
      setError((e as Error).message);
      return false;
    }
  };

  return (
    <Drawer
      title={
        <span>
          <span class="mono muted">{props.card.id}</span> <span class="chip">{humanize(props.card.column)}</span>
        </span>
      }
      onClose={() => props.onClose()}
    >
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show
        when={editing()}
        fallback={
          <div class="stack" style={{ gap: "6px" }}>
            <h2 style={{ margin: 0 }}>{props.card.title}</h2>
            <p style={{ margin: 0, "white-space": "pre-wrap" }}>{props.card.body || <span class="muted">No description.</span>}</p>
            <div>
              <button
                class="small"
                onClick={() => {
                  setTitle(props.card.title);
                  setBody(props.card.body);
                  setEditing(true);
                }}
              >
                Edit
              </button>
            </div>
          </div>
        }
      >
        <label>
          Title
          <input value={title()} onInput={(e) => setTitle(e.currentTarget.value)} />
        </label>
        <label>
          Body
          <textarea value={body()} onInput={(e) => setBody(e.currentTarget.value)} />
        </label>
        <div class="row">
          <button class="primary" onClick={async () => (await save({ title: title(), body: body() })) && setEditing(false)}>
            Save
          </button>
          <button onClick={() => setEditing(false)}>Cancel</button>
        </div>
      </Show>

      <div class="row">
        <label style={{ flex: 1 }}>
          Column
          <select value={props.card.column} onChange={(e) => void props.onMove(e.currentTarget.value)}>
            <For each={props.columns}>{(c) => <option value={c}>{humanize(c)}</option>}</For>
          </select>
        </label>
        <label style={{ flex: 1 }}>
          Assignee
          <select value={props.card.assignee ?? ""} onChange={(e) => void save({ assignee: e.currentTarget.value || null })}>
            <option value="">unassigned</option>
            <option value="operator">operator</option>
            <For each={projectAgents()}>{(a) => <option value={a.name}>{a.name}</option>}</For>
          </select>
        </label>
      </div>

      <dl class="kv">
        <dt>Labels</dt>
        <dd>{props.card.labels.join(", ") || "—"}</dd>
        <dt>Depends on</dt>
        <dd>{props.card.depends_on.join(", ") || "—"}</dd>
        <dt>Branch</dt>
        <dd class="mono small">{props.card.branch ?? "—"}</dd>
        <dt>Created</dt>
        <dd>
          {props.card.created_by} · {ago(props.card.created_at)}
        </dd>
        <Show when={props.card.links.length}>
          <dt>Links</dt>
          <dd>{props.card.links.map((l) => `${l.kind}:${l.ref}`).join(", ")}</dd>
        </Show>
      </dl>

      <section class="stack" style={{ gap: "8px" }}>
        <h3 style={{ margin: 0 }}>Comments</h3>
        <For each={props.card.comments} fallback={<p class="muted">No comments.</p>}>
          {(c) => (
            <div class="comment">
              <div class="small muted">
                <strong style={{ color: "var(--text)" }}>{c.author}</strong> · {ago(c.created_at)}
              </div>
              <div style={{ "white-space": "pre-wrap" }}>{c.body}</div>
            </div>
          )}
        </For>
        <form
          class="stack"
          style={{ gap: "6px" }}
          onSubmit={async (e) => {
            e.preventDefault();
            if (!comment().trim()) return;
            try {
              props.onSaved(await api.cards.comment(props.card.id, comment().trim()));
              setComment("");
            } catch (err) {
              setError((err as Error).message);
            }
          }}
        >
          <textarea value={comment()} onInput={(e) => setComment(e.currentTarget.value)} placeholder="Comment as operator" aria-label="Comment" />
          <div>
            <button class="primary" type="submit" disabled={!comment().trim()}>
              Comment
            </button>
          </div>
        </form>
      </section>

      <details>
        <summary>History</summary>
        <ul class="small">
          <For each={history()} fallback={<li class="muted">No events.</li>}>
            {(e) => (
              <li>
                {ago(e.at)}: {e.summary}
              </li>
            )}
          </For>
        </ul>
      </details>
    </Drawer>
  );
}
