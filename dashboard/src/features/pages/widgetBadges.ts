import type { ProjectCounts } from "../../api/types";
import { tabBadge, type TabBadge } from "../projects/tabCounts";

/** Registry metadata, not widget kinds or permission grants. */
export type WidgetBadgeSource = "agents" | "board" | "dashboard";

/** Each count source contributes once per page, regardless of instance count. */
export function pageWidgetBadge(
  sources: readonly WidgetBadgeSource[],
  blockedAgentCount: number,
  counts: ProjectCounts | null,
): TabBadge | null {
  return combineWidgetBadges(sources, source => tabBadge(source, blockedAgentCount, counts));
}

export function combineWidgetBadges(sources: readonly WidgetBadgeSource[], resolve: (source: WidgetBadgeSource) => TabBadge | null): TabBadge | null {
  const badges = [...new Set(sources)]
    .map(resolve)
    .filter((badge): badge is TabBadge => badge !== null);
  if (!badges.length) return null;
  return {
    n: badges.reduce((total, badge) => total + badge.n, 0),
    cls: badges.some(badge => badge.cls === "nav-count warn") ? "nav-count warn" : "nav-count info",
    text: badges.map(badge => badge.text).join(", "),
  };
}
