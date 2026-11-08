// @vitest-environment jsdom
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { describe, expect, it } from "vitest";
import type { Board, Card } from "../../api/types";
import { LanesBoard } from "./LanesBoard";
import { buildLanes } from "./lanes";

describe("board render budget", () => {
  for (const total of [294, 2940]) {
    it(`bounds initial rendering at ${total} cards`, () => {
      // Same ratio as the reported board: two thirds Done, already hidden
      // by both the old and new screens. Measure DOM work, not browser paint.
      const cards: Card[] = Array.from({ length: total / 3 }, (_, i) => ({
        id: `TER-${i}`, project: "p", column: "ready", position: i,
        title: "Board performance fixture", labels: [], body: "b".repeat(950),
        comments: [], links: [], depends_on: [], blocked_by: [], archived: false,
        created_by: "operator", created_at: "", updated_at: "",
      }));
      const board: Board = { project: "p", cards, columns: [{ name: "ready", position: 0, count: cards.length }] };
      const host = document.createElement("div");
      document.body.append(host);
      const start = performance.now();
      const dispose = render(() => createComponent(LanesBoard, {
        board, lanes: buildLanes(cards, false), phone: false, collapsed: [],
        onToggle: () => {}, visible: () => true, addButton: () => null, onDrop: () => {},
        card: c => {
          const el = document.createElement("article");
          el.dataset.card = c.id;
          el.textContent = c.title;
          return el;
        },
      }), host);
      const ms = performance.now() - start;
      const nodes = host.querySelectorAll("[data-card]").length;
      console.log(`board DOM total=${total} active=${cards.length} nodes=${nodes} render_ms=${ms.toFixed(2)}`);
      try {
        expect(nodes).toBe(cards.length);
        expect(ms).toBeLessThan(2000);
      } finally { dispose(); host.remove(); }
    });
  }
});
