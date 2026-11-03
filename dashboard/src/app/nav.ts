// The sidebar items and the routes that moved. Pure, so tests can read them.

export const PROJECT_ITEMS = [
  // The monitors of the project as blocks (docs/api.md §30); the first item, and the page of /p/:project.
  { path: "dashboard", label: "Dashboard" },
  // The CI/CD pipelines of the project and their runs (docs/api.md §31).
  { path: "pipelines", label: "Pipelines" },
  { path: "board", label: "Board" },
  // The repos of the project (PLAN §9a). In the tree a node, not a link: open, it shows the repos, each a link to its browser.
  // The page /p/:project/repos stays (other links go to it).
  { path: "repos", label: "Repos" },
  // The logs of the project (PROJECT_LOGS_TABS): every message and every event that touches it.
  { path: "logs", label: "Logs" },
  // The wiki and the memory of the project (KNOWLEDGE_TABS).
  { path: "knowledge", label: "Knowledge" },
  // Every agent of the project, in any status. In the tree a node, not a link: open, it shows the agents.
  // Last, so the agent rows of the open node do not push the pages down. The page /p/:project/agents stays.
  { path: "agents", label: "Agents" },
] as const;

/**
 * The tabs of the board screen, in order. Each is a route under
 * /p/:project/board. The Cards tab has the empty path: it is the board at
 * /p/:project/board, so a `?card=` link opens the card drawer.
 */
export const BOARD_TABS = [
  { path: "", label: "Cards" },
  { path: "artifacts", label: "Artifacts" },
  // The test runs of the project (docs/api.md, test runs); `tests/:id` is the page of one run.
  { path: "tests", label: "Tests" },
  // The schedules and triggers of the project; the page keeps its own `?tab=triggers` sub-tab.
  { path: "schedules", label: "Schedules" },
] as const;

/** The tabs of the knowledge screen, in order. Each is a route under /p/:project/knowledge. */
export const KNOWLEDGE_TABS = [
  // The wiki pages; the rest of the path is the page path.
  { path: "wiki", label: "Wiki" },
  { path: "memory", label: "Memory" },
  // The project DB: record types and records; `db/:type` is the records of one type.
  { path: "db", label: "DB" },
] as const;

/**
 * The tabs of the project settings screen, in order. Each is a route under
 * /p/:project/settings. The cog of the project row in the tree opens it.
 */
export const PROJECT_SETTINGS_TABS = [
  // The project edit and delete, and the Targets panel. The repos have their own item (PROJECT_ITEMS).
  { path: "general", label: "General" },
  // The project's secrets and the global ones it can use (PLAN §16b).
  { path: "secrets", label: "Secrets" },
  // The templates that apply to the project: its own and the global ones.
  { path: "templates", label: "Templates" },
] as const;

/**
 * Above the project tree. The Projects root of the tree links /projects.
 * The agents are in the tree only, under the Agents node of their project.
 * The /agents page stays, but it has no item.
 */
export const TOP_ITEMS = [
  { href: "/", label: "Home", end: true },
  // Everything that waits for the operator; the badge counts the open items.
  { href: "/inbox", label: "Inbox" },
  // The logs screen (LOGS_TABS): mail, events, and audit. Active on every /logs/... tab.
  { href: "/logs", label: "Logs" },
  { href: "/schedules", label: "Schedules" },
] as const;

/** The Settings section at the bottom of the sidebar: one link to the tabbed settings screen. */
export const SETTINGS_ITEMS = [{ href: "/settings", label: "Settings" }] as const;

/**
 * The tabs of the settings screen, in order. Each is a route under
 * /settings. The Skills tab also has the editor of one skill at
 * /settings/skills/:name.
 */
export const SETTINGS_TABS = [
  // The sign-in session, the password, and the log-outs.
  { path: "account", label: "Account" },
  { path: "subscriptions", label: "Subscriptions" },
  // The secret manager (PLAN §16b): every secret the caller manages.
  { path: "secrets", label: "Secrets" },
  // The global templates and the templates of each project.
  { path: "templates", label: "Templates" },
  // The system prompt library in the config repo (docs/api.md §28).
  { path: "system-prompts", label: "System prompts" },
  // The skill library in the config repo. The rest of the path is the skill name.
  { path: "skills", label: "Skills" },
  { path: "targets", label: "Targets" },
  // The config repo: for the operator or a sysadmin.
  { path: "harness", label: "Harness config" },
  { path: "hosts", label: "Hosts" },
] as const;

/** The tabs of the logs screen, in order. Each is a route under /logs. */
export const LOGS_TABS = [
  // The mail log: a record, with no unread state.
  { path: "mail", label: "Mail" },
  { path: "events", label: "Events" },
  { path: "audit", label: "Audit" },
] as const;

/** The tabs of the project logs screen. Each is a route under /p/:project/logs and shows that project only. */
export const PROJECT_LOGS_TABS = [
  { path: "mail", label: "Mail" },
  { path: "events", label: "Events" },
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
 * - The project inbox page is now the Mail tab of the project logs.
 * - Subscriptions, Hosts, and Config (now "Harness config") are tabs of the
 *   settings screen. A quota mail of the daemon links `/subscriptions`, so
 *   the old paths stay as redirects. `/settings` opens the first tab.
 * - Mail, Event log, and Audit are tabs of the logs screen, and the project
 *   Mail page is the Mail tab of the project logs. A bookmark can have an old
 *   path with a query (`/mail?thread=...`), so the old paths stay as
 *   redirects. `/logs` and `/p/:project/logs` open the Mail tab.
 * - The global Agents page is removed. The agents of a project are on the
 *   Agents page of the project. `/agents` opens the project cards.
 * - The project Secrets and Templates pages, and the project Settings page
 *   (now "General"), are tabs of the project settings screen. A
 *   bookmark can have an old path, so the old paths stay as redirects.
 *   `/p/:project/settings` opens the first tab. The Repos panel left the
 *   General tab for the Repos item of the project (/p/:project/repos); the
 *   General tab keeps its path.
 * - The project Artifacts page is a tab of the board screen (BOARD_TABS).
 * - The Versions tab of the board is gone: the Cards tab manages versions
 *   (version lanes, the Version modal, the toolbar version menu). The daemon
 *   puts `/p/<project>/versions/<id>` in `Version.url` and in the review
 *   mail, so the old paths, and the paths of the old tab, redirect in one
 *   hop to `/p/<project>/board?version=<id>`, which opens the
 *   version.
 * - The project Schedules page is a tab of the board screen (BOARD_TABS).
 * - The Tests tab of the board shows the test runs. A short link
 *   `/p/<project>/tests[/<id>]` goes to the tab.
 *   A schedule link of an old mail or a bookmark has the old path with a
 *   query (`?open=...&tab=triggers`), so the old path stays as a redirect.
 * - The project Wiki and Memory pages are tabs of the knowledge screen
 *   (KNOWLEDGE_TABS). The wiki redirect keeps the page path (`*path`).
 *   `/p/:project/knowledge` opens the Wiki tab.
 * - The global Secrets, Templates, Skills, and Targets pages are tabs of the
 *   settings screen. A bookmark can have an old path, and a link can have a
 *   query (`/targets?project=...`), so the old paths stay as redirects. The
 *   skill redirect keeps the skill name.
 */
export const MOVED_ROUTES = [
  { path: "/p/:project/spend", to: "/" },
  { path: "/questions", to: "/inbox" },
  { path: "/p/:project/inbox", to: "/p/:project/logs/mail" },
  { path: "/subscriptions", to: "/settings/subscriptions" },
  { path: "/hosts", to: "/settings/hosts" },
  { path: "/config", to: "/settings/harness" },
  { path: "/settings", to: "/settings/account" },
  { path: "/mail", to: "/logs/mail" },
  { path: "/events", to: "/logs/events" },
  { path: "/audit", to: "/logs/audit" },
  { path: "/logs", to: "/logs/mail" },
  { path: "/p/:project/mail", to: "/p/:project/logs/mail" },
  { path: "/p/:project/logs", to: "/p/:project/logs/mail" },
  { path: "/agents", to: "/projects" },
  { path: "/p/:project/secrets", to: "/p/:project/settings/secrets" },
  { path: "/p/:project/templates", to: "/p/:project/settings/templates" },
  { path: "/p/:project/settings", to: "/p/:project/settings/general" },
  { path: "/p/:project/versions", to: "/p/:project/board" },
  { path: "/p/:project/versions/:id", to: "/p/:project/board?version=:id" },
  { path: "/p/:project/board/versions", to: "/p/:project/board" },
  { path: "/p/:project/board/versions/:id", to: "/p/:project/board?version=:id" },
  { path: "/p/:project/artifacts", to: "/p/:project/board/artifacts" },
  { path: "/p/:project/artifacts/:id", to: "/p/:project/board/artifacts/:id" },
  { path: "/p/:project/schedules", to: "/p/:project/board/schedules" },
  { path: "/p/:project/tests", to: "/p/:project/board/tests" },
  { path: "/p/:project/tests/:id", to: "/p/:project/board/tests/:id" },
  { path: "/p/:project/wiki/*path", to: "/p/:project/knowledge/wiki/*path" },
  { path: "/p/:project/memory", to: "/p/:project/knowledge/memory" },
  { path: "/p/:project/knowledge", to: "/p/:project/knowledge/wiki" },
  { path: "/secrets", to: "/settings/secrets" },
  { path: "/templates", to: "/settings/templates" },
  { path: "/skills", to: "/settings/skills" },
  { path: "/skills/:name", to: "/settings/skills/:name" },
  { path: "/targets", to: "/settings/targets" },
] as const;

/**
 * The redirect target with the query of the old URL. A `to` with its own query keeps its keys; the old query
 * adds the keys that it does not have.
 */
export function withQuery(to: string, search: string): string {
  const old = search.replace(/^\?/, "");
  if (!old) return to;
  const at = to.indexOf("?");
  if (at < 0) return `${to}?${old}`;
  const merged = new URLSearchParams(to.slice(at + 1));
  for (const [k, v] of new URLSearchParams(old)) if (!merged.has(k)) merged.set(k, v);
  return `${to.slice(0, at)}?${merged.toString()}`;
}

/**
 * The new place of a moved path, or undefined. A `/*name` splat at the end
 * of a path takes the rest of the path, which can be empty. The rest goes
 * into the same splat of `to`, with no slash when it is empty.
 */
export function movedTo(pathname: string): string | undefined {
  for (const r of MOVED_ROUTES) {
    const names: string[] = [];
    const re = new RegExp(
      `^${r.path.replace(/\/\*([a-z]+)$|:([a-z]+)/g, (_m, splat: string | undefined, name: string | undefined) => {
        names.push((splat ?? name)!);
        return splat ? "(?:/(.*?))?" : "([^/]+)";
      })}/?$`,
    );
    const m = re.exec(pathname);
    if (!m) continue;
    const values = Object.fromEntries(names.map((n, i) => [n, m[i + 1] ?? ""]));
    return r.to.replace(/\/\*([a-z]+)$|:([a-z]+)/g, (_x, splat: string | undefined, name: string | undefined) =>
      splat ? (values[splat] ? `/${values[splat]}` : "") : (values[name!] ?? ""),
    );
  }
  return undefined;
}
