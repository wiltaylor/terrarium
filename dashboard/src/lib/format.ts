// Small formatting helpers. Pure functions, no Solid imports.

/** "3m", "2h 05m", "4d 3h". Negative or zero gives "now". */
export function duration(ms: number): string {
  if (ms <= 0) return "now";
  const min = Math.floor(ms / 60_000);
  if (min < 1) return `${Math.max(1, Math.floor(ms / 1000))}s`;
  if (min < 60) return `${min}m`;
  const h = Math.floor(min / 60);
  if (h < 24) return `${h}h ${String(min % 60).padStart(2, "0")}m`;
  const d = Math.floor(h / 24);
  return `${d}d ${h % 24}h`;
}

export const timeUntil = (iso: string, now = Date.now()): string => duration(Date.parse(iso) - now);

export function ago(iso: string | undefined, now = Date.now()): string {
  if (!iso) return "never";
  const ms = now - Date.parse(iso);
  if (ms < 30_000) return "just now";
  return `${duration(ms)} ago`;
}

/** Severity of a quota window against the subscription's alert threshold. */
export function usageLevel(pct: number, alertAt: number): "ok" | "warn" | "full" {
  if (pct >= 100) return "full";
  if (pct >= alertAt) return "warn";
  return "ok";
}

export function bytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

export function compact(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

export const humanize = (s: string): string => s.replace(/_/g, " ");

/** SVG path `d` for a sparkline of values 0..100 in a w x h box. */
export function sparkPath(values: number[], w: number, h: number): string {
  if (values.length === 0) return "";
  const step = values.length > 1 ? w / (values.length - 1) : 0;
  return values
    .map((v, i) => {
      const y = h - (Math.max(0, Math.min(100, v)) / 100) * h;
      return `${i === 0 ? "M" : "L"}${(i * step).toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
}
