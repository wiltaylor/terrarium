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
  // The project's secrets and the global ones it can use (PLAN §16b).
  { path: "secrets", label: "Secrets" },
  { path: "settings", label: "Settings & repos" },
] as const;

/** Above the project tree. */
export const TOP_ITEMS = [
  { href: "/", label: "Home", end: true },
  // Everything that waits for the operator; the badge counts the open items.
  { href: "/inbox", label: "Inbox" },
] as const;

/** The "Terrarium" section, under the project tree. The Projects root of the tree links /projects. */
export const GLOBAL_ITEMS = [
  // A tree root (app/AgentTree.tsx): it opens to show every agent.
  { href: "/agents", label: "Agents" },
  { href: "/templates", label: "Templates" },
  // The skill library in the config repo.
  { href: "/skills", label: "Skills" },
  // The secret manager (PLAN §16b): every secret the caller manages.
  { href: "/secrets", label: "Secrets" },
  // The mail log: a record, with no unread state.
  { href: "/mail", label: "Mail" },
  { href: "/targets", label: "Targets" },
  { href: "/schedules", label: "Schedules" },
  { href: "/events", label: "Event log" },
  { href: "/audit", label: "Audit" },
] as const;

/** The Settings section at the bottom of the sidebar: one link to the tabbed settings screen. */
export const SETTINGS_ITEMS = [{ href: "/settings", label: "Settings" }] as const;

/** The tabs of the settings screen, in order. Each is a route under /settings. */
export const SETTINGS_TABS = [
  // The sign-in session, the password, and the log-outs.
  { path: "account", label: "Account" },
  { path: "subscriptions", label: "Subscriptions" },
  // The config repo: for the operator or a sysadmin.
  { path: "harness", label: "Harness config" },
  { path: "hosts", label: "Hosts" },
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
 * - Subscriptions, Hosts, and Config (now "Harness config") are tabs of the
 *   settings screen. A quota mail of the daemon links `/subscriptions`, so
 *   the old paths stay as redirects. `/settings` opens the first tab.
 */
export const MOVED_ROUTES = [
  { path: "/p/:project/spend", to: "/" },
  { path: "/questions", to: "/inbox" },
  { path: "/p/:project/inbox", to: "/p/:project/mail" },
  { path: "/subscriptions", to: "/settings/subscriptions" },
  { path: "/hosts", to: "/settings/hosts" },
  { path: "/config", to: "/settings/harness" },
  { path: "/settings", to: "/settings/account" },
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
