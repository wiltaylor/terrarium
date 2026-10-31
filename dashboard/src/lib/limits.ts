// Resource limits of a target (docs/api.md §17.2, "Resource limits"): the
// same checks and the same short text as `terrarium_core::limits`. The
// create form checks a value before it goes out, the mock API applies the
// daemon's rules, and the target list shows the chip ("2 CPU · 4G"). The
// daemon stays the judge: it answers `422 invalid` for a bad value.

import type { HostCapabilities, LimitsInfo, TargetKind, TargetLimits } from "../api/types";

/** The limits as plain numbers (`terrarium_core::limits::Limits`). */
export interface ResolvedLimits {
  /** CPU time in thousandths of one core: 2000 is two cores. */
  cpuMillis?: number;
  memoryBytes?: number;
  swapBytes?: number;
  pids?: number;
  ioWeight?: number;
}

export const MIN_MEMORY = 16 * 1024 * 1024;
export const MAX_PIDS = 4_194_304;
const UNITS: Record<string, number> = { k: 2 ** 10, m: 2 ** 20, g: 2 ** 30, t: 2 ** 40 };

/** A byte count: a whole number, or a number with K, M, G, or T (powers of 1024). `4GiB` and `4GB` are `4G`. */
export function parseBytes(v: number | string): number | null {
  if (typeof v === "number") return Number.isSafeInteger(v) && v >= 0 ? v : null;
  const m = /^(\d+(?:\.\d+)?)\s*([kmgt])?(?:i?b)?$/i.exec(v.trim());
  if (!m) return null;
  const n = Number(m[1]);
  if (!m[2]) return Number.isSafeInteger(n) ? n : null;
  const b = Math.floor(n * (UNITS[m[2].toLowerCase()] ?? 1));
  return Number.isSafeInteger(b) ? b : null;
}

/** Check the limits and give them as numbers, or the sentence of the `422`. */
export function resolveLimits(l: TargetLimits): { limits: ResolvedLimits } | { error: string } {
  const out: ResolvedLimits = {};
  if (l.cpu !== undefined && l.cpu !== null) {
    const cores = typeof l.cpu === "number" ? l.cpu : Number(String(l.cpu).trim());
    if (!Number.isFinite(cores) || cores <= 0 || cores > 1024) return { error: `bad \`limits.cpu\` ${JSON.stringify(l.cpu)}: give the cores as a number from 0.01 to 1024, for example 2 or 0.5` };
    const pct = cores * 100;
    if (Math.abs(pct - Math.round(pct)) > 1e-6 || Math.round(pct) < 1) return { error: `bad \`limits.cpu\` ${JSON.stringify(l.cpu)}: use at most two decimals (0.01 core steps)` };
    out.cpuMillis = Math.round(pct) * 10;
  }
  if (l.memory !== undefined && l.memory !== null) {
    const b = parseBytes(l.memory);
    if (b === null) return { error: `bad \`limits.memory\` ${JSON.stringify(l.memory)}: use a number with K, M, G, or T, for example "512M" or "4G"` };
    if (b < MIN_MEMORY) return { error: `bad \`limits.memory\` ${JSON.stringify(l.memory)}: give at least 16M` };
    out.memoryBytes = b;
  }
  if (l.swap !== undefined && l.swap !== null) {
    if (out.memoryBytes === undefined) return { error: "`limits.swap` needs `limits.memory`: swap comes on top of it" };
    const b = parseBytes(l.swap);
    if (b === null) return { error: `bad \`limits.swap\` ${JSON.stringify(l.swap)}: use a number with K, M, G, or T` };
    out.swapBytes = b;
  }
  const whole = (v: number | string) => (typeof v === "number" ? v : /^\d+$/.test(v.trim()) ? Number(v) : NaN);
  if (l.pids !== undefined && l.pids !== null) {
    const n = whole(l.pids);
    if (!Number.isInteger(n) || n < 1 || n > MAX_PIDS) return { error: `bad \`limits.pids\` ${JSON.stringify(l.pids)}: a whole number from 1 to ${MAX_PIDS}` };
    out.pids = n;
  }
  if (l.io_weight !== undefined && l.io_weight !== null) {
    const n = whole(l.io_weight);
    if (!Number.isInteger(n) || n < 1 || n > 10000) return { error: `bad \`limits.io_weight\` ${JSON.stringify(l.io_weight)}: a whole number from 1 to 10000` };
    out.ioWeight = n;
  }
  return { limits: out };
}

/** A byte count in the largest unit that holds it whole: `4G`, `1536M`. */
export function showBytes(b: number): string {
  for (const [u, n] of [["T", 2 ** 40], ["G", 2 ** 30], ["M", 2 ** 20], ["K", 2 ** 10]] as const) if (b !== 0 && b % n === 0) return `${b / n}${u}`;
  return String(b);
}

/** The short text of the chip, as `Limits::summary`: `2 CPU · 4G · 512 pids · io 200`. */
export function limitsSummary(l: ResolvedLimits): string {
  const parts: string[] = [];
  if (l.cpuMillis !== undefined) parts.push(`${l.cpuMillis / 1000} CPU`);
  if (l.memoryBytes !== undefined) parts.push(l.swapBytes ? `${showBytes(l.memoryBytes)} (+${showBytes(l.swapBytes)} swap)` : showBytes(l.memoryBytes));
  if (l.pids !== undefined) parts.push(`${l.pids} pids`);
  if (l.ioWeight !== undefined) parts.push(`io ${l.ioWeight}`);
  return parts.join(" · ");
}

/** The kinds that take `limits`: a host target (each holder gets a budget) and a docker target with `image` (one budget for the container). */
export function limitsKindError(kind: TargetKind, hasImage: boolean): string | null {
  if (kind === "host") return null;
  if (kind === "docker") return hasImage ? null : "`limits` on a docker target needs `image`: Docker sets the limits when Terrarium runs the container";
  if (kind === "vmlab") return "`limits` are not for a vmlab target: the lab file sets the size of each VM (cpus, memory)";
  if (kind === "ssh") return "`limits` are not for an ssh target: use a host agent there and a `host` target";
  return `\`limits\` are not for a \`${kind}\` target: it has only a display`;
}

/** The `limits_info` of a view, as the daemon makes it, without the host agent's `problem`. */
export function limitsInfo(kind: TargetKind, l: TargetLimits | undefined): LimitsInfo | undefined {
  if (!l) return undefined;
  const r = resolveLimits(l);
  if ("error" in r) return undefined;
  return { summary: limitsSummary(r.limits), per: kind === "docker" ? "container" : "holder" };
}

/** The chip text of a target: the daemon's `limits_info.summary`, else the summary of `limits`. Null when it has none. */
export function limitsChip(t: { kind: TargetKind; limits?: TargetLimits; limits_info?: LimitsInfo }): string | null {
  return t.limits_info?.summary ?? limitsInfo(t.kind, t.limits)?.summary ?? null;
}

/** The tooltip of the chip: what the budget covers, and the problem of the host agent. */
export function limitsTitle(t: { kind: TargetKind; limits_info?: LimitsInfo }): string {
  const per = t.kind === "docker" ? "One budget for the container: all its agents share it." : "Each agent on the target gets this budget for all its sessions.";
  return t.limits_info?.problem ? `${per} ${t.limits_info.problem}` : per;
}

/** The chip of a host: can its host agent apply limits on host targets (docs/host-protocol.md, "Resource limits")? Null for an older host agent. */
export function hostLimitsChip(h: { capabilities?: HostCapabilities }): { label: string; ok: boolean; title: string } | null {
  const l = h.capabilities?.limits;
  if (!l) return null;
  if (!l.systemd) return { label: "no limits", ok: false, title: `Host targets on this host cannot have resource limits: ${l.reason ?? "systemd-run --user does not work"}` };
  return { label: "limits", ok: true, title: `systemd-run --user works; the user manager has the controllers: ${l.controllers?.join(", ") || "none"}` };
}
