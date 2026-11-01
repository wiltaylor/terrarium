// The Cards tab with version rows (`?rows=version`, lanes.ts): one lane for each version that is not
// archived, and a last lane "No version". On a wide screen the lanes are rows of one grid, under one row of
// column heads. On a phone (one column at a time) each column holds the lanes as sections that collapse; a
// phone has no drag and drop, so the Versions field of the card drawer moves a card between lanes.

import { createSignal, For, type JSX, Show } from "solid-js";
import type { Board, Card, Column, Version } from "../../api/types";
import { cardsIn } from "../../stores/board";
import { columnTitle, overLimit } from "./columns";
import { alsoIn, type Lane, laneCells, NO_VERSION_LANE } from "./lanes";
import { VERSION_CHIP } from "./versions";
import { humanize } from "../../lib/format";

export interface LaneDrag {
  draggable: boolean;
  dragging: boolean;
  onDragStart: (e: DragEvent) => void;
  onDragEnd: () => void;
}

type Drop = { lane: string; column: string; index: number };

export function LanesBoard(props: {
  board: Board;
  lanes: Lane[];
  versions: Version[];
  phone: boolean;
  collapsed: string[];
  onToggle: (laneKey: string) => void;
  visible: (c: Card) => boolean;
  /** The column with the "+" (new card) button. */
  addColumn?: string;
  addButton: (col: Column) => JSX.Element;
  card: (c: Card, lane: Lane, drag: LaneDrag, also: Version[]) => JSX.Element;
  onOpenVersion: (id: string) => void;
  /** A drop: `index` is the place in the lane cell, without the dragged card. */
  onDrop: (cardId: string, fromLane: string, toLane: string, column: string, index: number) => void;
  boardRef?: (el: HTMLDivElement) => void;
  onScroll?: () => void;
}) {
  const [dragging, setDragging] = createSignal<{ id: string; lane: string } | null>(null);
  const [drop, setDrop] = createSignal<Drop | null>(null);

  const dropIndex = (cell: HTMLElement, y: number) => {
    const cards = [...cell.querySelectorAll<HTMLElement>("[data-card]")].filter((el) => el.dataset.card !== dragging()?.id);
    const i = cards.findIndex((el) => {
      const r = el.getBoundingClientRect();
      return y < r.top + r.height / 2;
    });
    return i < 0 ? cards.length : i;
  };
  const isCollapsed = (key: string) => props.collapsed.includes(key);
  const laneName = (l: Lane) => l.version?.name ?? "No version";
  const visibleCount = (l: Lane) => l.cards.filter(props.visible).length;

  /** One cell: the cards of a lane in a column, with the drop handlers. */
  const Cell = (p: { lane: Lane; col: Column }) => {
    const cards = () => laneCells(p.lane, p.col.name);
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
          const index = dropIndex(e.currentTarget, e.clientY);
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
          <span class="lane-cell-count muted small" title={`${cards().length} of the ${cardsIn(props.board, p.col.name).length} cards of ${columnTitle(p.col)}`}>
            {cards().length}
          </span>
        </Show>
        <For each={cards()}>
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
                alsoIn(c.id, p.lane.key, props.versions),
              )}
            </>
          )}
        </For>
        <Show when={isDrop() && drop()!.index >= cards().filter((x) => x.id !== dragging()?.id).length}>
          <div class="drop-marker" />
        </Show>
      </div>
    );
  };

  /** The head of a lane: the collapse chevron, the version (opens its modal), the status, and the card count. */
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
      <Show when={p.lane.version} fallback={<span class="lane-name muted">No version</span>}>
        {(v) => (
          <>
            <button type="button" class="link lane-name" title={`Open version ${v().name}`} onClick={() => props.onOpenVersion(v().id)}>
              {v().name}
            </button>
            <span class={`chip ${VERSION_CHIP[v().status]}`}>{humanize(v().status)}</span>
          </>
        )}
      </Show>
      <span class="muted small">{visibleCount(p.lane) === 1 ? "1 card" : `${visibleCount(p.lane)} cards`}</span>
    </div>
  );

  const ColumnHead = (p: { col: Column }) => {
    const n = () => cardsIn(props.board, p.col.name).length;
    const over = () => overLimit(p.col, n());
    return (
      <div class="column-head lanes-col-head" classList={{ "over-limit": over() }} data-column-head={p.col.name}>
        <span>{columnTitle(p.col)}</span>
        <span class="row column-head-end">
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
              <section id={`board-col-${col.name}`} data-column={col.name} class="column lanes-column" classList={{ "over-limit": overLimit(col, cardsIn(props.board, col.name).length) }} aria-label={columnTitle(col)}>
                <ColumnHead col={col} />
                <For each={props.lanes}>
                  {(lane) => (
                    <section class="lane lane-phone" data-lane={lane.key}>
                      <LaneHead lane={lane} />
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
      <div class="lanes" style={{ "--lane-cols": String(props.board.columns.length) }} role="grid" aria-label="Cards by version">
        <div class="lanes-heads">
          <For each={props.board.columns}>{(col) => <ColumnHead col={col} />}</For>
        </div>
        <For each={props.lanes}>
          {(lane) => (
            <section class="lane" classList={{ "lane-none": lane.key === NO_VERSION_LANE, collapsed: isCollapsed(lane.key) }} data-lane={lane.key} aria-label={laneName(lane)}>
              <LaneHead lane={lane} />
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
