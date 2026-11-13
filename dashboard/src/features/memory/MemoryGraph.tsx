import { createMemo, createUniqueId, For } from "solid-js";
import type { MemoryGraph as Graph } from "../../api/types";
import { forceLayout } from "../../lib/force";

const W = 640;
const H = 480;

export function MemoryGraph(props: { graph: Graph; center: string; onPick: (id: string) => void }) {
  const markerId = `memory-arrow-${createUniqueId()}`;
  const layout = createMemo(() =>
    forceLayout(
      props.graph.nodes.map((n) => n.id),
      props.graph.edges,
      { width: W, height: H },
      { [props.center]: { x: W / 2, y: H / 2 } },
    ),
  );
  const pos = (id: string) => layout()[id] ?? { x: W / 2, y: H / 2 };
  const short = (s: string) => (s.length > 28 ? `${s.slice(0, 27)}…` : s);

  return (
    <svg class="graph" viewBox={`0 0 ${W} ${H}`} role="img" aria-label="Memory graph, neighbours to depth 1">
      <defs>
        <marker id={markerId} viewBox="0 0 10 10" refX="22" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
          <path d="M0,0 L10,5 L0,10 z" fill="currentColor" opacity="0.6" />
        </marker>
      </defs>
      <For each={props.graph.edges}>
        {(l) => {
          const a = () => pos(l.from);
          const b = () => pos(l.to);
          return (
            <g>
              <line class={l.relation} x1={a().x} y1={a().y} x2={b().x} y2={b().y} marker-end={`url(#${markerId})`} />
              <text class="edge-label" x={(a().x + b().x) / 2} y={(a().y + b().y) / 2 - 3} text-anchor="middle">
                {l.relation}
              </text>
            </g>
          );
        }}
      </For>
      <For each={props.graph.nodes}>
        {(n) => (
          <g
            class={`node${n.id === props.center ? " center" : ""}${n.superseded_by ? " superseded" : ""}`}
            transform={`translate(${pos(n.id).x},${pos(n.id).y})`}
            onClick={() => props.onPick(n.id)}
            tabIndex={0}
            onKeyDown={(e) => e.key === "Enter" && props.onPick(n.id)}
          >
            <title>{n.title}</title>
            <circle r="12" />
            <text y="26" text-anchor="middle">
              {n.id}
            </text>
            <text y="39" text-anchor="middle" opacity="0.75">
              {short(n.title)}
            </text>
          </g>
        )}
      </For>
    </svg>
  );
}
