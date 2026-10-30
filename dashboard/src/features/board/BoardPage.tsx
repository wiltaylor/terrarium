import { A, useParams, useSearchParams } from "@solidjs/router";
import { createEffect, createResource, createSignal, For, on, onCleanup, Show } from "solid-js";
import { createPagedList } from "../../components/paged";
import { HoldBadge } from "../../components/HoldBadge";
import { StatusBadge } from "../../components/StatusBadge";
import { ConfirmButton, Drawer, ErrorBanner, LoadMore, Modal, PageHead } from "../../components/ui";
import type { Card, Question } from "../../api/types";
import { ago, compact, humanize } from "../../lib/format";
import { countdown, doneSummary, etaStopped, resolveEta, tickInterval } from "../../lib/eta";
import { cardsIn, createBoardStore } from "../../stores/board";
import { cardSessionRows, commentSessions, sessionHref, sessionRange } from "./sessions";
import { CardQuestions } from "./CardQuestions";
import { type CardQuestionBadge, cardQuestionBadges, isCardQuestionEvent, linkable } from "./questions";
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

  // The open questions of the project: the "? n" badges of the cards and the "Link a question" list of the drawer.
  const [questions, setQuestions] = createSignal<Question[]>([]);
  let qGen = 0;
  const loadQuestions = async () => {
    const mine = ++qGen;
    const project = params.project;
    try {
      const rows = await api.questions.list({ project, status: "open" });
      if (mine === qGen) setQuestions(rows);
    } catch {
      // A caller who may not list questions gets no badges.
      if (mine === qGen) setQuestions([]);
    }
  };
  createEffect(on(() => params.project, () => void loadQuestions()));
  const badges = () => cardQuestionBadges(questions());

  // One shared clock for the ETA countdowns: 1 s while a countdown is under an hour, else 30 s; no timer without one.
  const [now, setNow] = createSignal(Date.now());
  createEffect(() => {
    const delay = tickInterval(board.state.board?.cards ?? [], now());
    if (delay === null) return;
    // A clock that is late (no ETA for a while) ticks at once.
    const t = setTimeout(() => setNow(Date.now()), Math.max(0, delay - (Date.now() - now())));
    onCleanup(() => clearTimeout(t));
  });

  let reloadTimer: ReturnType<typeof setTimeout> | undefined;
  let questionTimer: ReturnType<typeof setTimeout> | undefined;
  // Live: card and board events of this project reload the board; question events reload the badges.
  const off = events.on((e) => {
    if ((e.kind === "card" || e.kind === "board") && e.project === params.project && !dragging()) {
      clearTimeout(reloadTimer);
      reloadTimer = setTimeout(() => void board.reload(), 300);
    }
    if (isCardQuestionEvent(e, { project: params.project })) {
      clearTimeout(questionTimer);
      questionTimer = setTimeout(() => void loadQuestions(), 300);
    }
  });
  onCleanup(() => {
    off();
    clearTimeout(reloadTimer);
    clearTimeout(questionTimer);
  });

  const labels = () => [...new Set(board.state.board?.cards.flatMap((c) => c.labels) ?? [])].sort();
  const assignees = () => [...new Set(board.state.board?.cards.map((c) => c.assignee_name ?? c.assignee).filter((a): a is string => !!a) ?? [])].sort();
  const visible = (c: Card) => (!label() || c.labels.includes(label())) && (!assignee() || (c.assignee_name ?? c.assignee) === assignee());

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
          <input placeholder={`New ${board.prefix() ?? ""} card title`.replace("  ", " ")} value={newTitle()} onInput={(e) => setNewTitle(e.currentTarget.value)} aria-label="New card title" />
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
                            now={now()}
                            questions={badges().get(card.id)}
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
            now={now()}
            columns={board.state.board?.columns.map((x) => x.name) ?? []}
            linkable={linkable(questions(), params.project)}
            onQuestionsChanged={() => void loadQuestions()}
            onClose={() => setSearch({ card: undefined })}
            onMove={(column, comment) => {
              const b = board.state.board;
              const end = b ? cardsIn(b, column).filter((x) => x.id !== c().id).length : 0;
              return commitMove(c().id, column, end, comment);
            }}
            onSaved={board.put}
            onArchived={() => {
              board.drop(c().id);
              setSearch({ card: undefined });
            }}
          />
        )}
      </Show>
    </>
  );
}

/** The countdown chip of a card with an ETA, with a thin bar of the time used. Nothing for a done or archived card. */
function EtaChip(props: { card: Card; now: number }) {
  const cd = () => countdown(props.card, props.now);
  return (
    <Show when={cd()}>
      {(c) => {
        const soon = () => !c().overdue && c().progress !== null && c().progress! >= 0.9;
        return (
          <div class={`eta${c().overdue ? " overdue" : soon() ? " soon" : ""}`} title={c().title} role="timer" aria-label={`${c().label}. ${c().title}`}>
            <span class={`chip${c().overdue ? " bad" : soon() ? " warn" : " info"}`}>{c().label}</span>
            <Show when={c().progress !== null}>
              <div class="eta-bar" aria-hidden="true">
                <span style={{ width: `${Math.round(c().progress! * 100)}%` }} />
              </div>
            </Show>
          </div>
        );
      }}
    </Show>
  );
}

function KCard(props: {
  card: Card;
  now: number;
  questions?: CardQuestionBadge;
  hidden: boolean;
  dragging: boolean;
  onOpen: () => void;
  onDragStart: (e: DragEvent) => void;
  onDragEnd: () => void;
}) {
  const owner = () => (props.card.assignee ? agents.byId(props.card.assignee) : undefined);
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
        <span class="row" style={{ gap: "4px" }}>
          <Show when={props.questions}>
            {(q) => (
              <span class={`${q().cls} q-badge`} title={q().title} aria-label={q().title}>
                {q().label}
              </span>
            )}
          </Show>
          <Show when={(props.card.blocked_by ?? []).length}>
            <span class="chip warn" title={`Waiting on ${props.card.blocked_by.join(", ")}`}>
              waits on {props.card.blocked_by.length}
            </span>
          </Show>
        </span>
      </div>
      <div class="title">{props.card.title}</div>
      <EtaChip card={props.card} now={props.now} />
      <div class="row small">
        <For each={props.card.labels}>{(l) => <span class="chip">{l}</span>}</For>
      </div>
      <Show when={props.card.assignee}>
        <div class="spread small">
          <span>{owner()?.name ?? props.card.assignee_name ?? props.card.assignee}</span>
          <Show when={owner()}>
            {(a) => (
              <>
                <HoldBadge agentId={a().id} />
                <StatusBadge status={a().status} waitingOn={a().waiting_on} />
              </>
            )}
          </Show>
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
  now: number;
  columns: string[];
  linkable: Question[];
  onQuestionsChanged: () => void;
  onClose: () => void;
  onMove: (column: string, comment?: string) => Promise<void>;
  onSaved: (c: Card) => void;
  onArchived: () => void;
}) {
  const [comment, setComment] = createSignal("");
  const [error, setError] = createSignal<unknown>(null);
  const [editing, setEditing] = createSignal(false);
  const [title, setTitle] = createSignal("");
  const [body, setBody] = createSignal("");
  const [eta, setEta] = createSignal("");
  const etaCheck = () => (eta().trim() ? resolveEta(eta(), props.now) : null);
  const etaError = () => {
    const r = etaCheck();
    return r && "error" in r ? r.error : null;
  };
  const history = createPagedList(
    () => props.card.id,
    (id, cursor) => api.cards.events(id, { cursor }),
    (e) => e.id,
  );
  const projectAgents = () => agents.inProject(props.card.project);
  // docs/api.md §8 `GET /api/cards/{id}/sessions`; it loads again when the card changes (a new comment).
  // An older daemon without it: the sessions of the comments.
  const [sessions] = createResource(
    () => [props.card.id, props.card.updated_at, props.card.comments.length] as const,
    ([id]) => api.cards.sessions(id).catch(() => null),
  );
  const sessionRows = () => cardSessionRows(sessions.latest ?? commentSessions(props.card), props.card);
  // docs/api.md §16.3 `GET /api/cards/{id}/usage`. A daemon without it gives no line.
  const [usage] = createResource(
    () => props.card.id,
    (id) => api.usage.card(id).catch(() => null),
  );
  const usageLine = () => {
    const u = usage.latest;
    if (!u) return null;
    if (!u.total.messages) return "No usage recorded on this card.";
    const tok = (t: { input_tokens: number; output_tokens: number }) => compact(t.input_tokens + t.output_tokens);
    return `${tok(u.total)} tokens: ${u.models.map((m) => `${m.model} ${tok(m)}`).join(", ")}`;
  };

  const save = async (patch: Parameters<typeof api.cards.update>[1]) => {
    try {
      props.onSaved(await api.cards.update(props.card.id, patch));
      setError(null);
      return true;
    } catch (e) {
      setError(e);
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
            <For each={projectAgents()}>{(a) => <option value={a.id}>{a.name}</option>}</For>
          </select>
        </label>
      </div>

      <dl class="kv">
        <dt>Labels</dt>
        <dd>{props.card.labels.join(", ") || "—"}</dd>
        <dt>Depends on</dt>
        <dd>
          {props.card.depends_on.join(", ") || "—"}
          <Show when={(props.card.blocked_by ?? []).length}>
            {" "}
            <span class="chip warn">not done: {props.card.blocked_by.join(", ")}</span>
          </Show>
        </dd>
        <dt>Repo</dt>
        <dd class="mono small">{props.card.repo ?? "—"}</dd>
        <dt>Branch</dt>
        <dd class="mono small">{props.card.branch ?? "—"}</dd>
        <dt>ETA</dt>
        <dd class="stack" style={{ gap: "4px" }}>
          <Show
            when={etaStopped(props.card)}
            fallback={
              <Show when={props.card.eta_at} fallback={<span class="muted">—</span>}>
                <EtaChip card={props.card} now={props.now} />
                <span class="small muted">
                  {new Date(props.card.eta_at!).toLocaleString()}
                  {props.card.eta_set_by_name || props.card.eta_set_by ? ` · set by ${props.card.eta_set_by_name ?? props.card.eta_set_by}` : ""}
                  {props.card.eta_set_at ? ` ${ago(props.card.eta_set_at, props.now)}` : ""}
                </span>
              </Show>
            }
          >
            <span class="small">{doneSummary(props.card) ?? (props.card.eta_at ? `estimated ${new Date(props.card.eta_at).toLocaleString()}` : "—")}</span>
          </Show>
          <form
            class="eta-edit"
            onSubmit={async (e) => {
              e.preventDefault();
              if (!eta().trim() || etaError()) return;
              if (await save({ eta_at: eta().trim() })) setEta("");
            }}
          >
            <Show when={!etaStopped(props.card)}>
              <input value={eta()} onInput={(e) => setEta(e.currentTarget.value)} placeholder="2h30m" aria-label="ETA: a duration such as 45m, 2h30m, or 1d" aria-invalid={!!etaError()} />
              <button class="small" type="submit" disabled={!eta().trim() || !!etaError()}>
                {props.card.eta_at ? "Change ETA" : "Set ETA"}
              </button>
            </Show>
            <Show when={props.card.eta_at}>
              <button class="small" type="button" onClick={() => void save({ eta_at: null })}>
                Clear ETA
              </button>
            </Show>
          </form>
          <Show when={etaError()}>
            <span class="small" style={{ color: "var(--danger)" }}>
              {etaError()}
            </span>
          </Show>
        </dd>
        <dt>Created</dt>
        <dd>
          {props.card.created_by_name ?? props.card.created_by} · {ago(props.card.created_at)}
        </dd>
        <Show when={props.card.links.length}>
          <dt>Links</dt>
          <dd>{props.card.links.map((l) => `${l.kind}:${l.ref}`).join(", ")}</dd>
        </Show>
        <Show when={usageLine()}>
          <dt>Usage</dt>
          <dd class="small" title="Input plus output tokens of the samples attributed to this card, by model">
            {usageLine()}
          </dd>
        </Show>
      </dl>

      <CardQuestions card={props.card} linkable={props.linkable} onChanged={() => props.onQuestionsChanged()} />

      <section class="stack" style={{ gap: "8px" }}>
        <h3 style={{ margin: 0 }}>Comments</h3>
        <For each={props.card.comments} fallback={<p class="muted">No comments.</p>}>
          {(c) => (
            <div class="comment">
              <div class="small muted">
                <strong style={{ color: "var(--text)" }}>{c.author_name ?? c.author}</strong> · {ago(c.created_at)}
                <Show when={c.session}>
                  {(s) => (
                    <>
                      {" · "}
                      <A href={sessionHref(s().id)} title={`${s().harness} session ${s().id}`}>
                        View session
                      </A>
                    </>
                  )}
                </Show>
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
              setError(err);
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

      <section class="stack" style={{ gap: "8px" }}>
        <h3 style={{ margin: 0 }}>Sessions</h3>
        <For each={sessionRows()} fallback={<p class="muted small">No agent session worked on this card.</p>}>
          {(r) => (
            <div class="row small" style={{ gap: "6px", "flex-wrap": "wrap" }}>
              <A href={r.href}>{r.agent}</A>
              <span class="muted">{r.session.harness}</span>
              <span class="muted" title={r.session.started_at}>
                {sessionRange(r.session)}
              </span>
              <Show when={r.comments}>
                <span class="muted">
                  · {r.comments} {r.comments === 1 ? "comment" : "comments"}
                </span>
              </Show>
              <For each={r.chips}>{(c) => <span class={`chip ${c.tone}`}>{c.label}</span>}</For>
            </div>
          )}
        </For>
      </section>

      <details>
        <summary>History</summary>
        <ul class="small">
          <For each={history.items()} fallback={<li class="muted">{history.initial() ? "Loading…" : "No events."}</li>}>
            {(e) => (
              <li>
                {ago(e.at)}: {e.summary}
                <Show when={typeof e.data.actor_session === "string" && (e.data.actor_session as string)}>
                  {(sid) => (
                    <>
                      {" · "}
                      <A href={sessionHref(sid())}>View session</A>
                    </>
                  )}
                </Show>
              </li>
            )}
          </For>
        </ul>
        <LoadMore hasMore={history.hasMore()} loading={history.loading()} onMore={history.loadMore} />
      </details>
      <div>
        <ConfirmButton
          label="Archive"
          onConfirm={async () => {
            try {
              await api.cards.archive(props.card.id);
              props.onArchived();
            } catch (e) {
              setError(e);
            }
          }}
        />
      </div>
    </Drawer>
  );
}
