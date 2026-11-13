// The Cards tab (lanes.ts): one plain lane, or, grouped by version, one lane for each version label and a last
// lane "No version". On a wide screen the lanes are rows of one grid, under one row of column heads. On a phone
// (one column at a time) each column holds the lanes as sections that collapse; a phone has no drag and drop, so
// the Labels field of the card drawer moves a card between lanes. The plain lane has no lane head.

import { createSignal, createUniqueId, For, type JSX, Show, onCleanup } from "solid-js";
import type { BoardGroup } from "../../api/types";
import { stepTiming } from "./stepTiming";
import type { Board, CardSummary as Card, Column } from "../../api/types";
import { cardsIn } from "../../stores/board";
import { columnTitle, overLimit } from "./columns";
import { alsoIn, isPlain, type Lane, laneCells, NO_VERSION_LANE } from "./lanes";
import { groupDetailsColumn, groupSections } from "./groups";

export interface LaneDrag {
  draggable: boolean;
  dragging: boolean;
  onDragStart: (e: DragEvent) => void;
  onDragEnd: () => void;
}

type Drop = { lane: string; column: string; index: number };

function StepTiming(props: { step: BoardGroup["steps"][number]; now: number }) {
  return <span> {stepTiming(props.step, props.now)}</span>;
}

export function LanesBoard(props: {
  board: Board;
  lanes: Lane[];
  phone: boolean;
  collapsed: string[];
  onToggle: (laneKey: string) => void;
  visible: (c: Card) => boolean;
  /** The column with the "+" (new card) button. */
  addColumn?: string;
  addButton: (col: Column) => JSX.Element;
  /** `also`: the other version names of the card, in a version lane. */
  card: (c: Card, lane: Lane, drag: LaneDrag, also: string[]) => JSX.Element;
  /** A drop: `index` is the place in the lane cell, without the dragged card. */
  onDrop: (cardId: string, fromLane: string, toLane: string, column: string, index: number) => void;
  boardRef?: (el: HTMLDivElement) => void;
  onScroll?: () => void;
  columnId?: (name: string) => string;
}) {
  const id = createUniqueId();
  const columnId = (name: string) => props.columnId?.(name) ?? `${id}-col-${name}`;
  const [dragging, setDragging] = createSignal<{ id: string; lane: string } | null>(null);
  const [drop, setDrop] = createSignal<Drop | null>(null);
  const [now, setNow] = createSignal(Date.now());
  const timer = setInterval(() => setNow(Date.now()), 1000);
  onCleanup(() => clearInterval(timer));
  const [limits, setLimits] = createSignal<Record<string, number>>({});
  const limit = (column: string) => limits()[column] ?? 50;
  const filteredColumn = (column: string) => cardsIn(props.board, column).filter(c => c.kind === "group" || props.visible(c));

  const dropIndex = (cell: HTMLElement, y: number, order: Card[]) => {
    const cards = [...cell.querySelectorAll<HTMLElement>("[data-card]")].filter((el) => el.dataset.card !== dragging()?.id);
    const i = cards.findIndex((el) => {
      const r = el.getBoundingClientRect();
      return y < r.top + r.height / 2;
    });
    const remaining = order.filter((c) => c.id !== dragging()?.id);
    // Dropping below the rendered window must not jump past hidden cards.
    return i < 0 ? (cards.length ? remaining.findIndex(c => c.id === cards.at(-1)!.dataset.card) + 1 : 0) : remaining.findIndex((c) => c.id === cards[i]!.dataset.card);
  };
  // The board that is not grouped: no lane head, and the lane cannot collapse.
  const plain = () => isPlain(props.lanes);
  const isCollapsed = (key: string) => !plain() && props.collapsed.includes(key);
  const laneName = (l: Lane) => l.version ?? (l.key === NO_VERSION_LANE ? "No version" : "Cards");
  const visibleCount = (l: Lane) => l.cards.filter((c) => c.kind !== "group" && props.visible(c)).length;

  /** One cell: the cards of a lane in a column, with the drop handlers. */
  const Cell = (p: { lane: Lane; col: Column }) => {
    const cards = () => laneCells(p.lane, p.col.name).filter((c) => c.kind !== "group");
    const filtered = () => {
      const ids = new Set(filteredColumn(p.col.name).slice(0, limit(p.col.name)).map(c => c.id));
      return laneCells(p.lane, p.col.name).filter(c => ids.has(c.id));
    };
    const isDrop = () => drop()?.lane === p.lane.key && drop()?.column === p.col.name;
    return (
      <div
        class="lane-cell"
        classList={{ drop: isDrop() }}
        data-lane-cell={`${p.lane.key}:${p.col.name}`}
        aria-label={`${laneName(p.lane)}, ${columnTitle(p.col)}`}
        onDragOver={(e) => {
          if (!dragging()) return;
          e.preventDefault();
          e.dataTransfer!.dropEffect = "move";
          const index = dropIndex(e.currentTarget, e.clientY, cards());
          const d = drop();
          if (!d || d.lane !== p.lane.key || d.column !== p.col.name || d.index !== index) setDrop({ lane: p.lane.key, column: p.col.name, index });
        }}
        onDragLeave={(e) => {
          if (!e.currentTarget.contains(e.relatedTarget as Node)) setDrop(null);
        }}
        onDrop={(e) => {
          e.preventDefault();
          const from = dragging();
          const d = drop();
          setDrop(null);
          setDragging(null);
          if (from && d) props.onDrop(from.id, from.lane, d.lane, d.column, d.index);
        }}
      >
        <Show when={cards().length}>
          <span class="lane-cell-count muted small" title={`${cards().length} of the ${cardsIn(props.board, p.col.name).filter((c) => c.kind !== "group").length} cards of ${columnTitle(p.col)}`}>
            {cards().length}
          </span>
        </Show>
        <For each={groupSections(filtered(), p.col.name)}>
          {(section) => <section classList={{ "board-group": !!section.group }}>
            <Show when={section.group}>{(group) => (
              <header class="board-group-head">
                <strong>{group().title}</strong>
                <Show when={groupDetailsColumn(group(), p.lane.cards.filter((c) => props.visible(c)), props.board.columns.map((c) => c.name)) === p.col.name}>
                  <span>{group().done}/{group().total}</span>
                  <progress aria-label={`${group().title} progress`} value={group().done} max={Math.max(1, group().total)} />
                  <Show when={group().eta_at}>{(eta) => <time dateTime={eta()}>ETA {new Date(eta()).toLocaleString()}</time>}</Show>
                  <Show when={group().status}><span>{group().status}</span></Show>
                  <ol><For each={group().steps}>{(step) => <li>
                    {step.name}: {step.state} {step.text}
                    <StepTiming step={step} now={now()} />
                  </li>}</For></ol>
                </Show>
              </header>
            )}</Show>
        <For each={section.cards}>
          {(c) => (
            <>
              <Show when={isDrop() && dragging()?.id !== c.id && cards().filter((x) => x.id !== dragging()?.id).findIndex((x) => x.id === c.id) === drop()?.index}>
                <div class="drop-marker" />
              </Show>
              {props.card(
                c,
                p.lane,
                {
                  draggable: !props.phone,
                  dragging: dragging()?.id === c.id && dragging()?.lane === p.lane.key,
                  onDragStart: (e) => {
                    e.dataTransfer!.setData("text/plain", c.id);
                    e.dataTransfer!.effectAllowed = "move";
                    setDragging({ id: c.id, lane: p.lane.key });
                  },
                  onDragEnd: () => {
                    setDragging(null);
                    setDrop(null);
                  },
                },
                alsoIn(c, p.lane.key),
              )}
            </>
          )}
        </For>
          </section>}
        </For>
        <Show when={isDrop() && drop()!.index >= cards().filter((x) => x.id !== dragging()?.id).length}>
          <div class="drop-marker" />
        </Show>
      </div>
    );
  };

  /** The head of a lane: the collapse chevron, the version, and the card count. */
  const LaneHead = (p: { lane: Lane }) => (
    <div class="lane-head" data-lane-head={p.lane.key}>
      <button
        type="button"
        class="ghost disclosure"
        aria-expanded={!isCollapsed(p.lane.key)}
        aria-label={`${isCollapsed(p.lane.key) ? "Expand" : "Collapse"} ${laneName(p.lane)}`}
        onClick={() => props.onToggle(p.lane.key)}
      >
        {isCollapsed(p.lane.key) ? "▸" : "▾"}
      </button>
      <span class="lane-name" classList={{ muted: !p.lane.version }} title={p.lane.version ? `The cards with the label ${p.lane.key}` : "The cards with no version label"}>
        {laneName(p.lane)}
      </span>
      <span class="muted small">{visibleCount(p.lane) === 1 ? "1 card" : `${visibleCount(p.lane)} cards`}</span>
    </div>
  );

  const ColumnHead = (p: { col: Column }) => {
    const n = () => cardsIn(props.board, p.col.name).filter((c) => c.kind !== "group").length;
    const over = () => overLimit(p.col, n());
    return (
      <div class="column-head lanes-col-head" classList={{ "over-limit": over() }} data-column-head={p.col.name}>
        <span>{columnTitle(p.col)}</span>
        <span class="row column-head-end">
          <Show when={filteredColumn(p.col.name).length > limit(p.col.name)}>
            <button class="small" onClick={() => setLimits(v => ({ ...v, [p.col.name]: limit(p.col.name) + 50 }))}>Show more ({filteredColumn(p.col.name).length - limit(p.col.name)})</button>
          </Show>
          <span class="count muted small" title={over() ? `Over the WIP limit of ${p.col.wip_limit}: no more cards enter until some leave` : undefined}>
            {n()}
            {p.col.wip_limit !== undefined ? ` / ${p.col.wip_limit}` : ""}
          </span>
          <Show when={p.col.name === props.addColumn}>{props.addButton(p.col)}</Show>
        </span>
      </div>
    );
  };

  return (
    <Show
      when={!props.phone}
      fallback={
        // A phone: the columns scroll as before (the chips choose one), and each column holds the lanes.
        <div class="board" ref={props.boardRef} onScroll={() => props.onScroll?.()}>
          <For each={props.board.columns}>
            {(col) => (
              <section id={columnId(col.name)} data-column={col.name} class="column lanes-column" classList={{ "over-limit": overLimit(col, cardsIn(props.board, col.name).filter((c) => c.kind !== "group").length) }} aria-label={columnTitle(col)}>
                <ColumnHead col={col} />
                <For each={props.lanes}>
                  {(lane) => (
                    <section class="lane lane-phone" classList={{ "lane-plain": plain() }} data-lane={lane.key}>
                      <Show when={!plain()}>
                        <LaneHead lane={lane} />
                      </Show>
                      <Show when={!isCollapsed(lane.key)}>
                        <Cell lane={lane} col={col} />
                      </Show>
                    </section>
                  )}
                </For>
              </section>
            )}
          </For>
        </div>
      }
    >
      <div class="lanes" style={{ "--lane-cols": String(props.board.columns.length) }} role="grid" aria-label={plain() ? "Cards" : "Cards by version"}>
        <div class="lanes-heads">
          <For each={props.board.columns}>{(col) => <ColumnHead col={col} />}</For>
        </div>
        <For each={props.lanes}>
          {(lane) => (
            <section class="lane" classList={{ "lane-none": lane.key === NO_VERSION_LANE, "lane-plain": plain(), collapsed: isCollapsed(lane.key) }} data-lane={lane.key} aria-label={laneName(lane)}>
              <Show when={!plain()}>
                <LaneHead lane={lane} />
              </Show>
              <Show when={!isCollapsed(lane.key)}>
                <For each={props.board.columns}>{(col) => <Cell lane={lane} col={col} />}</For>
              </Show>
            </section>
          )}
        </For>
      </div>
    </Show>
  );
}
