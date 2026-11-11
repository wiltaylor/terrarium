import { A, useNavigate, useParams, useSearchParams } from "@solidjs/router";
import { createEffect, createMemo, createResource, createSignal, For, on, onCleanup, onMount, Show } from "solid-js";
import { createStore } from "solid-js/store";
import { createPagedList } from "../../components/paged";
import { HoldBadge } from "../../components/HoldBadge";
import { ConfirmButton, Drawer, ErrorBanner, LoadMore, Modal, PageHead } from "../../components/ui";
import { LinkText } from "../../components/LinkText";
import type { Card, CardSummary, Column, NewCard, Question } from "../../api/types";
import { ago, compact } from "../../lib/format";
import { projectBase } from "../../lib/url";
import { countdown, doneSummary, etaStopped, resolveEta, tickInterval } from "../../lib/eta";
import { idleBadge } from "../../lib/idle";
import { cardState, type CardState, tileLabels } from "../../lib/cardState";
import { cardsIn, createBoardStore } from "../../stores/board";
import { cardSessionRows, commentSessions, sessionHref, sessionRange } from "./sessions";
import { CardQuestions } from "./CardQuestions";
import { CardFiles } from "./CardFiles";
import { CardTestsChip, LinkedRuns } from "../tests/parts";
import { type CardQuestionBadge, cardQuestionBadges, isCardQuestionEvent, linkable } from "./questions";
import { emptyNewCardForm, type NewCardErrors, type NewCardForm, newCardBody, newCardColumn, nextNewCardForm } from "./newCard";
import { columnTitle, isBlockedColumn } from "./columns";
import { ColumnsEditor } from "./ColumnsEditor";
import { editCardForm, editCardPatch, type EditCardForm } from "./editCard";
import { LanesBoard } from "./LanesBoard";
import { buildLanes, columnIndex, laneCells, laneMoveLabels, laneStateKey, readLaneState, toggleCollapsed } from "./lanes";
import { activeFilterCount, columnFromQuery, columnQuery, nearestColumn, PHONE_QUERY } from "./phone";
import { boardVersions, matchesVersion, NO_VERSION } from "./versionLabels";
import { cardAgentName } from "./cardAgent";
import { agents, api, events } from "../../stores";
import { boardView, doneToggleHref } from "./done";
import { BOARD_RELOAD_MS, createDebounce, DONE_RELOAD_MS, eventTouchesCard, RELOAD_MAX_WAIT_MS } from "./liveReload";

export function DoneCardsPage() {
  return <BoardPage done />;
}

export function BoardPage(props: { done?: boolean }) {
  const navigate = useNavigate();
  const params = useParams<{ project: string }>();
  const [search, setSearch] = useSearchParams<{ card?: string; version?: string; group?: string; col?: string }>();
  const board = createBoardStore(api);
  const view = createMemo(() => board.state.board ? boardView(board.state.board, props.done) : undefined);
  // The card that is in a drag (LanesBoard): no live reload of the board until the drop.
  const [dragging, setDragging] = createSignal<string | null>(null);
  const [pendingBlock, setPendingBlock] = createSignal<{ id: string; column: string; index: number } | null>(null);
  const [editColumns, setEditColumns] = createSignal(false);
  let editColumnsButton: HTMLButtonElement | undefined;
  const [label, setLabel] = createSignal("");
  const [assignee, setAssignee] = createSignal("");
  // The column of the open New card dialog, or null.
  const [newCardIn, setNewCardIn] = createSignal<string | null>(null);
  let addButton: HTMLButtonElement | undefined;
  const [filtersOpen, setFiltersOpen] = createSignal(false);

  // A phone (phone.ts PHONE_QUERY) shows one column at a time, and it has no drag and drop: the Column select of the card drawer moves a card.
  const phoneQuery = typeof matchMedia === "function" ? matchMedia(PHONE_QUERY) : undefined;
  const [phone, setPhone] = createSignal(phoneQuery?.matches ?? false);
  onMount(() => {
    if (!phoneQuery) return;
    const change = () => setPhone(phoneQuery.matches);
    phoneQuery.addEventListener("change", change);
    onCleanup(() => phoneQuery.removeEventListener("change", change));
  });

  createEffect(on(() => [params.project, props.done] as const, ([p, done]) => void board.load(p, done)));

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

  // The version view (versionLabels.ts, lanes.ts): `?version=<name>` shows only the cards with the label
  // `version:<name>` (`none`: the cards with no version label); `?group=version` shows a lane for each version
  // label. Both are in the URL, so a reload and a link keep them. An old `?rows=` in the URL has no effect.
  // localStorage keeps the collapsed lanes of each project.
  const storage = (): Storage | undefined => (typeof localStorage === "undefined" ? undefined : localStorage);
  const [laneState, setLaneState] = createSignal(readLaneState(null));
  createEffect(on(() => params.project, (p) => setLaneState(readLaneState(storage()?.getItem(laneStateKey(p)) ?? null))));
  const saveLaneState = (next: ReturnType<typeof readLaneState>) => {
    setLaneState(next);
    try {
      storage()?.setItem(laneStateKey(params.project), JSON.stringify(next));
    } catch {
      // A full or blocked storage: the choice lasts for this page only.
    }
  };
  const grouped = () => search.group === "version";
  const versionFilter = () => search.version?.trim() || undefined;
  const versionNames = createMemo(() => boardVersions(board.state.board?.cards ?? []));
  const lanes = createMemo(() => buildLanes(view()?.cards ?? [], grouped()));
  const [laneError, setLaneError] = createSignal<unknown>(null);

  /** A drop: in the same lane it moves the card; into another version lane it changes the card's version label first. */
  const onLaneDrop = async (id: string, fromLane: string, toLane: string, column: string, cellIndex: number) => {
    const b = board.state.board;
    if (!b) return;
    const lane = lanes().find((l) => l.key === toLane);
    const cell = lane ? laneCells(lane, column).filter((c) => c.id !== id && c.kind !== "group") : [];
    const index = columnIndex(cell, cardsIn(b, column).filter((c) => c.id !== id), cellIndex);
    const card = b.cards.find((c) => c.id === id);
    const labels = card ? laneMoveLabels(card.labels, fromLane, toLane) : null;
    if (!card || !labels) return void commitMove(id, column, index);
    try {
      board.put(await api.cards.update(id, { labels }));
      setLaneError(null);
    } catch (e) {
      setLaneError(e);
      return;
    }
    if (card.column !== column) await commitMove(id, column, index);
  };

  // One shared clock for the ETA countdowns: 1 s while a countdown is under an hour, else 30 s; no timer without one.
  const [now, setNow] = createSignal(Date.now());
  createEffect(() => {
    const delay = tickInterval(board.state.board?.cards ?? [], now());
    if (delay === null) return;
    // A clock that is late (no ETA for a while) ticks at once.
    const t = setTimeout(() => setNow(Date.now()), Math.max(0, delay - (Date.now() - now())));
    onCleanup(() => clearTimeout(t));
  });

  // Done can span many pages. Coalesce busy project bursts before fetching
  // that range, rather than requesting every page several times a second.
  // The max wait keeps a busy project from holding the reload back for good.
  const reloadBoard = createDebounce(() => void board.reload(), RELOAD_MAX_WAIT_MS);
  let detailTimer: ReturnType<typeof setTimeout> | undefined;
  let questionTimer: ReturnType<typeof setTimeout> | undefined;
  // Live events refresh summaries (including compact test chips and job state),
  // retaining the loaded Done range. Only the open drawer reloads full details.
  const off = events.on((e) => {
    if ((e.kind === "card" || e.kind === "board" || e.kind === "test_run" || e.kind === "job") && e.project === params.project && !dragging()) {
      reloadBoard.schedule(props.done ? DONE_RELOAD_MS : BOARD_RELOAD_MS);
      const card = search.card;
      if (card && eventTouchesCard(e, card)) {
        clearTimeout(detailTimer);
        detailTimer = setTimeout(() => {
          if (search.card === card) void reloadSelected();
        }, 300);
      }
    }
    if (isCardQuestionEvent(e, { project: params.project })) {
      clearTimeout(questionTimer);
      questionTimer = setTimeout(() => void loadQuestions(), 300);
    }
  });
  onCleanup(() => {
    off();
    reloadBoard.cancel();
    clearTimeout(detailTimer);
    clearTimeout(questionTimer);
  });

  const labels = () => [...new Set(board.state.board?.cards.flatMap((c) => c.labels) ?? [])].sort();
  const assignees = () => [...new Set(board.state.board?.cards.map((c) => c.assignee_name ?? c.assignee).filter((a): a is string => !!a) ?? [])].sort();
  const visible = (c: CardSummary) =>
    (!label() || c.labels.includes(label())) && (!assignee() || (c.assignee_name ?? c.assignee) === assignee()) && matchesVersion(c.labels, versionFilter());

  const boardColumns = (): Column[] => board.state.board?.columns ?? [];
  const titleOf = (name: string) => columnTitle(boardColumns().find((c) => c.name === name) ?? { name });
  const commitMove = async (id: string, column: string, index: number, comment?: string) => {
    // The column with the role `blocked` (columns.ts roleColumn) needs a comment.
    if (isBlockedColumn(boardColumns(), column) && !comment) {
      setPendingBlock({ id, column, index });
      return;
    }
    await board.move(id, column, index, comment);
  };

  const [detailError, setDetailError] = createSignal<unknown>(null);
  const [detail, { mutate: setSelected, refetch: reloadSelected }] = createResource(
    () => search.card ? [params.project, search.card] as const : false,
    async ([project, id]) => {
      setDetailError(null);
      try {
        const card = await api.cards.get(id);
        return card.project === project ? card : undefined;
      } catch (e) { setDetailError(e); return undefined; }
    },
  );
  const selected = () => detail()?.id.toUpperCase() === search.card?.toUpperCase() ? detail() : undefined;

  // The column of the phone board: `?col=` in the URL, so a reload and the back button keep it.
  // A tap on a column chip and a swipe on the board change it; a change of the URL scrolls the board to it.
  let boardEl: HTMLDivElement | undefined;
  let chipsEl: HTMLDivElement | undefined;
  const columnNames = () => view()?.columns.map((c) => c.name) ?? [];
  const activeColumn = () => columnFromQuery(search.col, columnNames());
  const setColumn = (name: string) => setSearch({ col: columnQuery(name, columnNames()) }, { replace: true });
  // The target of a scroll that the page started: the scroll events on the way to it do not change the column.
  let pinned: string | null = null;
  let pinTimer: ReturnType<typeof setTimeout> | undefined;
  const columnEls = () => [...(boardEl?.querySelectorAll<HTMLElement>("[data-column]") ?? [])];
  const showColumn = (name: string, smooth: boolean) => {
    const el = columnEls().find((c) => c.dataset.column === name);
    if (!boardEl || !el || Math.abs(boardEl.scrollLeft - el.offsetLeft) < 2) return;
    pinned = name;
    clearTimeout(pinTimer);
    pinTimer = setTimeout(() => (pinned = null), 1000);
    const reduce = typeof matchMedia === "function" && matchMedia("(prefers-reduced-motion: reduce)").matches;
    boardEl.scrollTo({ left: el.offsetLeft, behavior: smooth && !reduce ? "smooth" : "auto" });
  };
  let scrollFrame = 0;
  const onBoardScroll = () => {
    if (!phone()) return;
    cancelAnimationFrame(scrollFrame);
    scrollFrame = requestAnimationFrame(() => {
      if (!boardEl) return;
      const name = nearestColumn(
        boardEl.scrollLeft,
        columnEls().map((el) => ({ name: el.dataset.column ?? "", left: el.offsetLeft })),
      );
      if (!name) return;
      if (pinned) {
        if (name !== pinned) return;
        pinned = null;
      }
      if (name !== activeColumn()) setColumn(name);
    });
  };
  createEffect(
    on([activeColumn, phone, () => !!board.state.board], ([name, isPhone, loaded]) => {
      if (!isPhone || !name || !loaded) return;
      // After the columns render. A scroll that the page started to this column goes on.
      requestAnimationFrame(() => {
        if (pinned !== name) showColumn(name, false);
        // The chip row keeps the active chip in view. It scrolls only itself, not the page.
        const chip = chipsEl?.querySelector<HTMLElement>(`[data-column="${CSS.escape(name)}"]`);
        if (chipsEl && chip) chipsEl.scrollTo({ left: chip.offsetLeft - (chipsEl.clientWidth - chip.offsetWidth) / 2 });
      });
    }),
  );
  onCleanup(() => {
    clearTimeout(pinTimer);
    cancelAnimationFrame(scrollFrame);
  });

  const addCardButton = (col: Column) => (
    <button
      type="button"
      class="ghost column-add"
      ref={addButton}
      aria-label={`Add a card to ${columnTitle(col)}`}
      title={`Add a card to ${columnTitle(col)}`}
      aria-haspopup="dialog"
      onClick={() => setNewCardIn(col.name)}
    >
      <svg viewBox="0 0 16 16" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
        <path d="M8 3v10M3 8h10" />
      </svg>
    </button>
  );
  const filterCount = () => activeFilterCount({ label: label(), assignee: assignee(), version: versionFilter() ?? "" });

  return (
    <div class="board-page">
      <PageHead title={props.done ? "Done cards" : "Cards"} level={2}>
        <button type="button" onClick={() => navigate(doneToggleHref(projectBase(params.project), !!props.done, search))}>
          {props.done ? "Back to cards" : "Show done cards"}
        </button>
        {/* A phone folds the two filters into this button; a wider screen shows them in the row and hides the button. */}
        <button
          type="button"
          class="board-filters-toggle"
          aria-expanded={filtersOpen()}
          aria-controls="board-filters"
          classList={{ active: filterCount() > 0 }}
          onClick={() => setFiltersOpen((o) => !o)}
        >
          Filters
          <Show when={filterCount()}>
            <span class="nav-count" aria-label={`${filterCount()} active`}>
              {filterCount()}
            </span>
          </Show>
        </button>
        <div id="board-filters" class="board-filters" classList={{ open: filtersOpen() }}>
          <select value={label()} onChange={(e) => setLabel(e.currentTarget.value)} aria-label="Filter by label">
            <option value="">Any label</option>
            <For each={labels()}>{(l) => <option value={l}>{l}</option>}</For>
          </select>
          <select value={assignee()} onChange={(e) => setAssignee(e.currentTarget.value)} aria-label="Filter by assignee">
            <option value="">Any assignee</option>
            <For each={assignees()}>{(a) => <option value={a}>{a}</option>}</For>
          </select>
          <select value={versionFilter() ?? ""} onChange={(e) => setSearch({ version: e.currentTarget.value || undefined })} aria-label="Filter by version">
            <option value="">Any version</option>
            {/* A version in the URL that no card has (any more) stays selectable, so the select shows the filter. */}
            <For each={[...versionNames(), ...(versionFilter() && versionFilter() !== NO_VERSION && !versionNames().includes(versionFilter()!) ? [versionFilter()!] : [])]}>
              {(v) => <option value={v}>{v}</option>}
            </For>
            <option value={NO_VERSION}>No version</option>
          </select>
        </div>
        <label class="row small board-group">
          <input type="checkbox" checked={grouped()} onChange={(e) => setSearch({ group: e.currentTarget.checked ? "version" : undefined })} />
          Group by version
        </label>
        <button type="button" ref={editColumnsButton} aria-haspopup="dialog" disabled={!board.state.board} onClick={() => setEditColumns(true)}>
          Edit columns
        </button>
      </PageHead>
      <ErrorBanner error={board.state.error} onDismiss={board.clearError} />
      <ErrorBanner error={laneError()} onDismiss={() => setLaneError(null)} />

      <Show when={view()} fallback={<p class="muted">{board.state.loading ? "Loading…" : ""}</p>}>
        {(b) => (
          <>
            {/* The phone board shows one column; these chips choose it. A wider screen hides them. */}
            <div class="board-colnav" role="group" aria-label="Columns" ref={chipsEl}>
              <For each={b().columns}>
                {(col) => (
                  <button
                    type="button"
                    data-column={col.name}
                    aria-pressed={activeColumn() === col.name}
                    aria-controls={`board-col-${col.name}`}
                    classList={{ active: activeColumn() === col.name }}
                    onClick={() => {
                      showColumn(col.name, true);
                      setColumn(col.name);
                    }}
                  >
                    {columnTitle(col)}
                    {/* The cards that the filters show, so a phone finds the matches. */}
                    <span class="nav-count">{cardsIn(b(), col.name).filter((c) => c.kind !== "group" && visible(c)).length}</span>
                  </button>
                )}
              </For>
            </div>
            <LanesBoard
              board={b()}
              lanes={lanes()}
              phone={phone()}
              collapsed={laneState().collapsed}
              onToggle={(key) => saveLaneState(toggleCollapsed(laneState(), key))}
              visible={visible}
              addColumn={props.done ? undefined : newCardColumn(columnNames())}
              addButton={addCardButton}
              onDrop={(...a) => void onLaneDrop(...a)}
              boardRef={(el) => (boardEl = el)}
              onScroll={onBoardScroll}
              card={(card, lane, drag, also) => (
                <KCard
                  card={card}
                  now={now()}
                  questions={badges().get(card.id)}
                  hidden={!visible(card)}
                  dragging={drag.dragging}
                  draggable={drag.draggable}
                  note={also.length ? `also in ${also.join(", ")}` : undefined}
                  lane={lane.key}
                  onOpen={() => setSearch({ card: card.id })}
                  onDragStart={(e) => {
                    drag.onDragStart(e);
                    setDragging(card.id);
                  }}
                  onDragEnd={() => {
                    drag.onDragEnd();
                    setDragging(null);
                  }}
                />
              )}
            />
          </>
        )}
      </Show>

      <LoadMore hasMore={!!board.state.board?.next_cursor} loading={board.state.loading} onMore={() => void board.more()} />
      <ErrorBanner error={detailError()} />
      <Show when={newCardIn()}>
        {(column) => (
          <NewCardModal
            prefix={board.prefix()}
            column={column()}
            columns={boardColumns()}
            agents={agents.inProject(params.project)}
            onCreate={async (body) => {
              const card = await api.cards.create(params.project, body);
              board.put(card);
              return card;
            }}
            onClose={() => {
              setNewCardIn(null);
              // The focus goes back to the "+" button that opened the dialog.
              addButton?.focus();
            }}
          />
        )}
      </Show>

      <Show when={pendingBlock()}>
        {(p) => (
          <BlockModal
            cardId={p().id}
            column={titleOf(p().column)}
            onCancel={() => setPendingBlock(null)}
            onSubmit={async (comment) => {
              const { id, column, index } = p();
              setPendingBlock(null);
              await commitMove(id, column, index, comment);
            }}
          />
        )}
      </Show>

      <Show when={editColumns()}>
        <ColumnsEditor
          columns={boardColumns()}
          onSave={async (edits) => {
            const b = await api.board.update(params.project, edits);
            await board.reload();
            return b;
          }}
          onClose={() => {
            setEditColumns(false);
            editColumnsButton?.focus();
          }}
        />
      </Show>

      <Show when={selected()}>
        {(c) => (
          <CardDrawer
            card={c()}
            now={now()}
            columns={boardColumns()}
            linkable={linkable(questions(), params.project)}
            onQuestionsChanged={() => void loadQuestions()}
            onClose={() => setSearch({ card: undefined })}
            onMove={(column, comment) => {
              const b = board.state.board;
              const end = b ? cardsIn(b, column).filter((x) => x.id !== c().id).length : 0;
              return commitMove(c().id, column, end, comment).then(() => { void reloadSelected(); });
            }}
            onSaved={(card) => { board.put(card); setSelected(card); }}
            onArchived={() => {
              board.drop(c().id);
              setSearch({ card: undefined });
            }}
          />
        )}
      </Show>
    </div>
  );
}

/** The countdown chip of a card with an ETA, with a thin bar of the time used. Nothing for a done or archived card. */
function EtaChip(props: { card: CardSummary; now: number }) {
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
  card: CardSummary;
  now: number;
  questions?: CardQuestionBadge;
  /** A line under the title, for example "also in 1.4.0" in a version lane. */
  note?: string;
  /** The version lane of the card. */
  lane?: string;
  hidden: boolean;
  dragging: boolean;
  /** False on a phone: a touch scrolls the board, and the drawer moves the card. */
  draggable: boolean;
  onOpen: () => void;
  onDragStart: (e: DragEvent) => void;
  onDragEnd: () => void;
}) {
  const owner = () => (props.card.assignee ? agents.byId(props.card.assignee) : undefined);
  // The one state chip (TER-158): the open job, the review column, or the live status of the assignee.
  const state = () => cardState(props.card, owner()?.status);
  // An idle assignee with an idle mark: the chip shows the idle time and the alert (lib/idle.ts).
  const idle = () => idleBadge(props.card, props.now);
  const stateLabel = (s: CardState) => (s.kind === "idle" && idle() ? idle()!.label : s.label);
  const stateTitle = (s: CardState) => (s.kind === "idle" && idle() ? idle()!.title : s.title);
  return (
    <article
      class={`kcard${props.dragging ? " dragging" : ""}`}
      style={{ display: props.hidden ? "none" : undefined }}
      data-card={props.card.id}
      data-card-lane={props.lane}
      draggable={props.draggable}
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
          <Show when={state()?.kind !== "idle" && idle()}>
            {(b) => (
              <span class="chip warn idle-badge" title={b().title} aria-label={b().title}>
                {b().label}
              </span>
            )}
          </Show>
          <Show when={state()}>
            {(s) => (
              <span class={`chip ${s().tone} card-state`} data-state={s().kind} title={stateTitle(s())} aria-label={`${s().label}. ${stateTitle(s())}`}>
                {stateLabel(s())}
              </span>
            )}
          </Show>
        </span>
      </div>
      <div class="title">{props.card.title}</div>
      <Show when={props.note}>
        <div class="small muted kcard-note">{props.note}</div>
      </Show>
      <EtaChip card={props.card} now={props.now} />
      <div class="row small">
        <Show when={props.card.tests}>{(t) => <CardTestsChip project={props.card.project} tests={t()} />}</Show>
        <Show when={props.card.trouble?.length}>
          <span class="chip warn" title={props.card.trouble?.map((s) => s.replaceAll("_", " ")).join(", ")}>Trouble: {props.card.trouble?.length}</span>
        </Show>
        <Show when={props.card.comment_count}><span class="small muted" title={props.card.last_comment_at ?? undefined}>{props.card.comment_count} comments</span></Show>
        <For each={tileLabels(props.card.labels)}>{(l) => <span class="chip">{l}</span>}</For>
      </div>
      <Show when={props.card.assignee}>
        <div class="spread small">
          <span>{owner()?.name ?? props.card.assignee_name ?? props.card.assignee}</span>
          <Show when={owner()}>{(a) => <HoldBadge agentId={a().id} />}</Show>
        </div>
      </Show>
      <Show when={cardAgentName(props.card)}>
        {(n) => (
          <div class="small muted kcard-agent" title={`Card agent: the ephemeral agent bound to ${props.card.id}`}>
            agent {n()}
          </div>
        )}
      </Show>
    </article>
  );
}

/**
 * The New card dialog: the "+" button of the backlog column opens it.
 * "Create and add another" keeps it open for the next card (newCard.ts nextNewCardForm).
 */
function NewCardModal(props: {
  prefix: string | null;
  column: string;
  columns: Column[];
  agents: { id: string; name: string }[];
  onCreate: (body: NewCard) => Promise<Card>;
  onClose: () => void;
}) {
  const [form, setForm] = createStore<NewCardForm>(emptyNewCardForm(props.column));
  const [errors, setErrors] = createSignal<NewCardErrors>({});
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const [created, setCreated] = createSignal<string[]>([]);
  let titleEl: HTMLInputElement | undefined;
  onMount(() => titleEl?.focus());
  const field = (k: keyof NewCardForm) => (e: { currentTarget: { value: string } }) => setForm(k, e.currentTarget.value);

  const submit = async (another: boolean) => {
    if (busy()) return;
    const r = newCardBody(form);
    if ("errors" in r) {
      setErrors(r.errors);
      if (r.errors.title) titleEl?.focus();
      return;
    }
    setErrors({});
    setBusy(true);
    try {
      const card = await props.onCreate(r.body);
      setError(null);
      if (!another) return props.onClose();
      setCreated((c) => [...c, card.id]);
      setForm(nextNewCardForm(form));
      titleEl?.focus();
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal title="New card" class="modal-full" onClose={() => props.onClose()}>
      <form
        class="stack new-card"
        onSubmit={(e) => {
          e.preventDefault();
          void submit((e.submitter as HTMLElement | null)?.dataset.another === "1");
        }}
      >
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <Show when={created().length}>
          <p class="small muted" role="status" style={{ margin: 0 }}>
            Created: {created().join(", ")}
          </p>
        </Show>
        <label classList={{ "field-bad": !!errors().title }}>
          Title
          <input
            ref={titleEl}
            value={form.title}
            onInput={field("title")}
            placeholder={`New ${props.prefix ?? ""} card title`.replace("  ", " ")}
            aria-invalid={!!errors().title}
            aria-required="true"
          />
          <Show when={errors().title}>
            <span class="field-error small" role="alert">
              {errors().title}
            </span>
          </Show>
        </label>
        <label>
          Description (Markdown)
          <textarea
            value={form.body}
            onInput={field("body")}
            onKeyDown={(e) => {
              // Ctrl+Enter or Cmd+Enter creates the card from the description.
              if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
                e.preventDefault();
                void submit(false);
              }
            }}
          />
        </label>
        <div class="row">
          <label class="drawer-field">
            Column
            <select value={form.column} onChange={field("column")}>
              <For each={props.columns}>{(c) => <option value={c.name}>{columnTitle(c)}</option>}</For>
            </select>
          </label>
          <label class="drawer-field">
            Assignee
            <select value={form.assignee} onChange={field("assignee")}>
              <option value="">unassigned</option>
              <option value="operator">operator</option>
              <For each={props.agents}>{(a) => <option value={a.id}>{a.name}</option>}</For>
            </select>
          </label>
        </div>
        <div class="row">
          <label class="drawer-field">
            Labels (comma separated)
            <input value={form.labels} onInput={field("labels")} placeholder="bug, ui" />
          </label>
          <label class="drawer-field">
            Depends on (comma separated)
            <input value={form.dependsOn} onInput={field("dependsOn")} placeholder={`${props.prefix ?? "TER"}-1, ${props.prefix ?? "TER"}-2`} />
          </label>
        </div>
        <label classList={{ "field-bad": !!errors().eta }}>
          ETA (optional)
          <input value={form.eta} onInput={field("eta")} placeholder="2h30m" aria-label="ETA: a duration such as 45m, 2h30m, or 1d" aria-invalid={!!errors().eta} />
          <Show when={errors().eta}>
            <span class="field-error small" role="alert">
              {errors().eta}
            </span>
          </Show>
        </label>
        <div class="row">
          <button class="primary" type="submit" disabled={busy()}>
            Create card
          </button>
          <button type="submit" data-another="1" disabled={busy()}>
            Create and add another
          </button>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}

function BlockModal(props: { cardId: string; column: string; onCancel: () => void; onSubmit: (comment: string) => void }) {
  const [text, setText] = createSignal("");
  return (
    <Modal title={`Block ${props.cardId}`} onClose={() => props.onCancel()}>
      <p class="muted" style={{ margin: 0 }}>
        Moving a card to {props.column} (the blocked column) needs a comment. The daemon mails it to the operator and the card's creator.
      </p>
      <textarea value={text()} onInput={(e) => setText(e.currentTarget.value)} placeholder="What is needed to unblock it?" aria-label="Comment" />
      <div class="row">
        <button class="primary" disabled={!text().trim()} onClick={() => props.onSubmit(text().trim())}>
          Move to {props.column}
        </button>
        <button onClick={() => props.onCancel()}>Cancel</button>
      </div>
    </Modal>
  );
}

function CardDrawer(props: {
  card: Card;
  now: number;
  columns: Column[];
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
  const [edit, setEdit] = createSignal<EditCardForm>(editCardForm(props.card));
  const editField = (k: keyof EditCardForm) => (e: { currentTarget: { value: string } }) => setEdit({ ...edit(), [k]: e.currentTarget.value });
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
    () => [props.card.id, props.card.updated_at] as const,
    ([id]) => api.usage.card(id).catch(() => null),
  );
  const usageLine = () => {
    const u = usage.latest;
    if (!u) return null;
    const time = u.working_seconds == null ? "" : `, ${Math.floor(u.working_seconds / 60)} working minutes`;
    if (!u.total.messages) return `No token usage recorded on this card${time}.`;
    const tok = (t: { input_tokens: number; output_tokens: number }) => compact(t.input_tokens + t.output_tokens);
    return `${u.total.turns} turns, ${tok(u.total)} tokens${time}: ${u.models.map((m) => `${m.model} ${tok(m)}`).join(", ")}`;
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
          <span class="mono muted">{props.card.id}</span> <span class="chip">{columnTitle(props.columns.find((c) => c.name === props.card.column) ?? { name: props.card.column })}</span>
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
            <p style={{ margin: 0, "white-space": "pre-wrap" }}>
              <Show when={props.card.body} fallback={<span class="muted">No description.</span>}>
                <LinkText text={props.card.body} project={props.card.project} />
              </Show>
            </p>
            <div>
              <button
                class="small"
                onClick={() => {
                  setEdit(editCardForm(props.card));
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
          <input value={edit().title} onInput={editField("title")} />
        </label>
        <label>
          Body
          <textarea value={edit().body} onInput={editField("body")} />
        </label>
        <label>
          Labels
          <input value={edit().labels} onInput={editField("labels")} placeholder="bug, ui" />
        </label>
        <label>
          Depends on
          <input value={edit().dependsOn} onInput={editField("dependsOn")} placeholder="TER-1, TER-2 (empty: no dependencies)" />
        </label>
        <div class="row">
          <button
            class="primary"
            onClick={async () => {
              const patch = editCardPatch(props.card, edit());
              if (!Object.keys(patch).length || (await save(patch))) setEditing(false);
            }}
          >
            Save
          </button>
          <button onClick={() => setEditing(false)}>Cancel</button>
        </div>
      </Show>

      <div class="row">
        <label class="drawer-field">
          Column
          <select value={props.card.column} onChange={(e) => void props.onMove(e.currentTarget.value)}>
            <For each={props.columns}>{(c) => <option value={c.name}>{columnTitle(c)}</option>}</For>
          </select>
        </label>
        <label class="drawer-field">
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
        <Show when={usage()?.signals?.length}>
          <dt>Trouble</dt>
          <dd role="status">{usage()?.signals?.map((s) => s.replaceAll("_", " ")).join(", ")}</dd>
        </Show>
      </dl>

      <CardFiles card={props.card} />

      <CardQuestions card={props.card} linkable={props.linkable} onChanged={() => props.onQuestionsChanged()} />

      <LinkedRuns project={props.card.project} card={props.card.id} />

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
              <div style={{ "white-space": "pre-wrap" }}>
                <LinkText text={c.body} project={props.card.project} />
              </div>
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
