// The sidebar items and the routes that moved. Pure, so tests can read them.

export const PROJECT_ITEMS = [
  { path: "board", label: "Board" },
  { path: "inbox", label: "Inbox" },
  { path: "wiki", label: "Wiki" },
  { path: "memory", label: "Memory" },
  { path: "versions", label: "Versions" },
  { path: "artifacts", label: "Artifacts" },
  { path: "schedules", label: "Schedules" },
  { path: "settings", label: "Settings & repos" },
] as const;

export const GLOBAL_ITEMS = [
  { href: "/", label: "Home", end: true },
  { href: "/projects", label: "Projects" },
  { href: "/agents", label: "Agents" },
  { href: "/templates", label: "Templates" },
  { href: "/subscriptions", label: "Subscriptions" },
  { href: "/mail", label: "Mail" },
  { href: "/targets", label: "Targets" },
  { href: "/schedules", label: "Schedules" },
  { href: "/hosts", label: "Hosts" },
  { href: "/events", label: "Event log" },
  { href: "/audit", label: "Audit" },
  { href: "/settings", label: "Settings" },
] as const;

/**
 * Routes that are gone, and where they go now. The Spend page is gone: the
 * usage split by project, agent, model, and card is in the Home tile detail.
 * Budget alert mail sent before 2026-10-25 links `/p/<project>/spend`
 * (new mail links `/p/<project>`), so the route redirects to Home and does
 * not answer "Page not found".
 */
export const MOVED_ROUTES = [{ path: "/p/:project/spend", to: "/" }] as const;

/** The new place of a moved path, or undefined. */
export function movedTo(pathname: string): string | undefined {
  for (const r of MOVED_ROUTES) {
    const re = new RegExp(`^${r.path.replace(/:[a-z]+/g, "[^/]+")}/?$`);
    if (re.test(pathname)) return r.to;
  }
  return undefined;
}
