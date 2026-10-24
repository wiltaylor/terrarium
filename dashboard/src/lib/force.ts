// A small force-directed layout for the memory graph. Synchronous and
// deterministic: the same input gives the same positions.

export interface LayoutNode {
  id: string;
  x: number;
  y: number;
  /** Pinned nodes keep their position. */
  fixed?: boolean;
}

export interface LayoutOptions {
  width: number;
  height: number;
  iterations?: number;
  linkDistance?: number;
  repulsion?: number;
}

export function forceLayout(
  ids: string[],
  edges: { from: string; to: string }[],
  opts: LayoutOptions,
  pinned: Record<string, { x: number; y: number }> = {},
): Record<string, LayoutNode> {
  const { width, height } = opts;
  const iterations = opts.iterations ?? 300;
  const linkDistance = opts.linkDistance ?? Math.min(width, height) / 3.2;
  const repulsion = opts.repulsion ?? linkDistance * linkDistance * 0.9;
  const cx = width / 2;
  const cy = height / 2;

  // Start on a circle so the result is deterministic.
  const nodes: LayoutNode[] = ids.map((id, i) => {
    const p = pinned[id];
    if (p) return { id, x: p.x, y: p.y, fixed: true };
    const a = (2 * Math.PI * i) / Math.max(ids.length, 1);
    const r = Math.min(width, height) / 4;
    return { id, x: cx + r * Math.cos(a), y: cy + r * Math.sin(a) };
  });
  const index = new Map(nodes.map((n, i) => [n.id, i]));
  const links = edges
    .map((e) => [index.get(e.from), index.get(e.to)] as const)
    .filter((l): l is readonly [number, number] => l[0] !== undefined && l[1] !== undefined && l[0] !== l[1]);

  for (let it = 0; it < iterations; it++) {
    const cooling = 1 - it / iterations;
    const fx = new Float64Array(nodes.length);
    const fy = new Float64Array(nodes.length);

    for (let i = 0; i < nodes.length; i++) {
      for (let j = i + 1; j < nodes.length; j++) {
        let dx = nodes[i]!.x - nodes[j]!.x;
        let dy = nodes[i]!.y - nodes[j]!.y;
        let d2 = dx * dx + dy * dy;
        if (d2 < 0.01) {
          dx = (i - j) * 0.1;
          dy = 0.1;
          d2 = dx * dx + dy * dy;
        }
        const f = repulsion / d2;
        const d = Math.sqrt(d2);
        fx[i]! += (dx / d) * f;
        fy[i]! += (dy / d) * f;
        fx[j]! -= (dx / d) * f;
        fy[j]! -= (dy / d) * f;
      }
    }
    for (const [a, b] of links) {
      const dx = nodes[b]!.x - nodes[a]!.x;
      const dy = nodes[b]!.y - nodes[a]!.y;
      const d = Math.max(Math.sqrt(dx * dx + dy * dy), 0.01);
      const f = (d - linkDistance) * 0.08;
      fx[a]! += (dx / d) * f;
      fy[a]! += (dy / d) * f;
      fx[b]! -= (dx / d) * f;
      fy[b]! -= (dy / d) * f;
    }
    for (let i = 0; i < nodes.length; i++) {
      const n = nodes[i]!;
      if (n.fixed) continue;
      fx[i]! += (cx - n.x) * 0.02;
      fy[i]! += (cy - n.y) * 0.02;
      const step = 0.5 * cooling + 0.05;
      const mx = Math.max(-20, Math.min(20, fx[i]! * step));
      const my = Math.max(-20, Math.min(20, fy[i]! * step));
      n.x = Math.max(24, Math.min(width - 24, n.x + mx));
      n.y = Math.max(24, Math.min(height - 24, n.y + my));
    }
  }
  return Object.fromEntries(nodes.map((n) => [n.id, n]));
}
