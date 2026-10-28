// The sidebar items and the routes that moved. Pure, so tests can read them.

export const PROJECT_ITEMS = [
  // Every agent of the project, in any status.
  { path: "agents", label: "Agents" },
  // The templates that apply to the project: its own and the global ones.
  { path: "templates", label: "Templates" },
  { path: "board", label: "Board" },
  // The mail log of the project: every message that touches it.
  { path: "mail", label: "Mail" },
  { path: "wiki", label: "Wiki" },
  { path: "memory", label: "Memory" },
  { path: "versions", label: "Versions" },
  { path: "artifacts", label: "Artifacts" },
  { path: "schedules", label: "Schedules" },
  { path: "settings", label: "Settings & repos" },
] as const;

export const GLOBAL_ITEMS = [
  { href: "/", label: "Home", end: true },
  // Everything that waits for the operator; the badge counts the open items.
  { href: "/inbox", label: "Inbox" },
  { href: "/projects", label: "Projects" },
  { href: "/agents", label: "Agents" },
  { href: "/templates", label: "Templates" },
  // The skill library in the config repo.
  { href: "/skills", label: "Skills" },
  { href: "/subscriptions", label: "Subscriptions" },
  // The mail log: a record, with no unread state.
  { href: "/mail", label: "Mail" },
  { href: "/targets", label: "Targets" },
  { href: "/schedules", label: "Schedules" },
  { href: "/hosts", label: "Hosts" },
  { href: "/events", label: "Event log" },
  { href: "/audit", label: "Audit" },
  // The config repo: for the operator or a sysadmin.
  { href: "/config", label: "Config" },
  { href: "/settings", label: "Settings" },
] as const;

/**
 * Routes that are gone, and where they go now. A `:param` in `to` takes the
 * value of the same param in the old path.
 *
 * - The Spend page is gone: the usage split by project, agent, model, and
 *   card is in the Home tile detail. Budget alert mail sent before
 *   2026-10-25 links `/p/<project>/spend` (new mail links `/p/<project>`),
 *   so the route redirects to Home and does not answer "Page not found".
 * - The Questions page is now the Inbox, with notices, mail to the
 *   operator, and paused threads.
 * - The project inbox page is now the project Mail log.
 */
export const MOVED_ROUTES = [
  { path: "/p/:project/spend", to: "/" },
  { path: "/questions", to: "/inbox" },
  { path: "/p/:project/inbox", to: "/p/:project/mail" },
] as const;

/** The new place of a moved path, or undefined. */
export function movedTo(pathname: string): string | undefined {
  for (const r of MOVED_ROUTES) {
    const names: string[] = [];
    const re = new RegExp(
      `^${r.path.replace(/:([a-z]+)/g, (_m, name: string) => {
        names.push(name);
        return "([^/]+)";
      })}/?$`,
    );
    const m = re.exec(pathname);
    if (!m) continue;
    const values = Object.fromEntries(names.map((n, i) => [n, m[i + 1]!]));
    return r.to.replace(/:([a-z]+)/g, (_x, name: string) => values[name] ?? "");
  }
  return undefined;
}
