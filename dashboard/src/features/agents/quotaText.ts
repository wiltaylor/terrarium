// The quota line under the composers of the agent page (issue #22): one
// short muted line from `GET /api/usage/me?agent=` (docs/api.md §16.3), in
// the place of the old Quota panel. Pure; no Solid imports.

import type { UsageMe } from "../../api/types";
import { compact, orderWindows, subscriptionTitle, timeUntil, windowLabel, windowValue } from "../../lib/format";

/** A window name for one short line: the code (`5h`, `7d`, `7d opus`), else the long label (`Monthly`). */
export function shortWindow(window: string): string {
  return /^\d+[mhdw](?:_|$)/.test(window) ? window.replace(/_/g, " ") : windowLabel(window);
}

/**
 * The line, for example `Claude Ultra · 5h 42% (resets in 2h 10m) · 7d 18% ·
 * today 120.0k tokens, 34 turns (3% of the subscription)`. With no usage
 * numbers: the plan name only. `plan` is the agent's subscription name, for
 * when the daemon gave no subscription.
 */
export function quotaLine(me: UsageMe | null | undefined, plan: string, now: number): string {
  const sub = me?.subscription;
  const title = sub ? subscriptionTitle(sub) : plan;
  if (!me) return title;
  const parts = [title];
  for (const w of orderWindows(sub?.windows ?? [])) {
    parts.push(`${shortWindow(w.window)} ${windowValue(w)}${w.resets_at ? ` (resets in ${timeUntil(w.resets_at, now)})` : ""}`);
  }
  const mine = me.today.mine;
  const tokens = mine.input_tokens + mine.output_tokens;
  if (tokens > 0 || mine.turns > 0) {
    const share = me.today.share_pct !== undefined ? ` (${me.today.share_pct}% of the subscription)` : "";
    parts.push(`today ${compact(tokens)} tokens, ${mine.turns} ${mine.turns === 1 ? "turn" : "turns"}${share}`);
  }
  if (sub?.local_estimate_only && parts.length > 1) parts.push("local estimate");
  return parts.join(" · ");
}
