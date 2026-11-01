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

/** `$10.65`, or `10.65 EUR` for another currency. */
export function money(n: number, currency: string): string {
  return currency === "USD" ? `$${n.toFixed(2)}` : `${n.toFixed(2)} ${currency}`;
}

/** `$10.65 left of $60.00` for a credit balance; other currencies by code. */
export function balanceText(b: { total: number; remaining: number; currency: string }): string {
  return `${money(b.remaining, b.currency)} left of ${money(b.total, b.currency)}`;
}

/** The short value of a window: the money left for a credit balance, else the percentage. */
export function windowValue(w: { used_pct: number; balance?: { remaining: number; currency: string } }): string {
  return w.balance ? `${money(w.balance.remaining, w.balance.currency)} left` : `${Math.round(w.used_pct)}%`;
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

/** The length of a window name in seconds (`5h`, `7d`, `7d_fable`, `30m`, `2w`); `undefined` for `extra`, `credits`, `monthly`... */
export function windowSeconds(window: string): number | undefined {
  const m = /^(\d+)([mhdw])(?:_|$)/.exec(window);
  if (!m) return window === "monthly" ? 30 * 86_400 : undefined;
  const unit = { m: 60, h: 3_600, d: 86_400, w: 604_800 }[m[2] as "m" | "h" | "d" | "w"];
  return Number(m[1]) * unit;
}

/**
 * A clear label for a window name: `5h` is "5-hour", `7d` is "Weekly",
 * `7d_fable` is "Weekly, Fable" (a weekly limit of one model), `extra` is
 * "Extra usage". Other names show with spaces for underscores.
 */
export function windowLabel(window: string): string {
  const fixed: Record<string, string> = { extra: "Extra usage", monthly: "Monthly", credits: "Credits", balance: "Balance" };
  if (fixed[window]) return fixed[window];
  const m = /^(\d+)([mhdw])(?:_(.+))?$/.exec(window);
  if (!m) return humanize(window);
  const n = Number(m[1]);
  const unit = m[2];
  const base = unit === "d" && n === 7 ? "Weekly" : unit === "w" && n === 1 ? "Weekly" : unit === "d" && n === 1 ? "Daily" : `${n}-${{ m: "minute", h: "hour", d: "day", w: "week" }[unit as "m" | "h" | "d" | "w"]}`;
  if (!m[3]) return base;
  const scope = humanize(m[3]).replace(/\boauth\b/i, "OAuth");
  return `${base}, ${scope.charAt(0).toUpperCase()}${scope.slice(1)}`;
}

/**
 * The windows in reading order: the shortest first (a window with no
 * length last), the window of all models before a per-model one of the
 * same length. The first is the most relevant window: the 5-hour one
 * when the plan has it, else the weekly one (an Anthropic Ultra plan).
 */
export function orderWindows<W extends { window: string }>(windows: readonly W[]): W[] {
  const len = (w: W) => windowSeconds(w.window) ?? Number.POSITIVE_INFINITY;
  const scoped = (w: W) => (/^\d+[mhdw]_/.test(w.window) ? 1 : 0);
  return [...windows].sort((a, b) => len(a) - len(b) || scoped(a) - scoped(b) || a.window.localeCompare(b.window));
}

/** The most relevant window of a plan: the first of `orderWindows`, or `undefined` when there is none. */
export const primaryWindow = <W extends { window: string }>(windows: readonly W[]): W | undefined => orderWindows(windows)[0];

/** The name of a subscription for people: `display_name`, else the plan, else the name (an older daemon). */
export const subscriptionTitle = (s: { name: string; display_name?: string; plan?: string }): string => s.display_name ?? s.plan ?? s.name;
