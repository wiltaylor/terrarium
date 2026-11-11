// The sidebar items and the routes that moved. Pure, so tests can read them.

/**
 * The project shell tabs, in order: the one tab row of a project. The first
 * path segment selects the tab. The board has no tab row of its own (TER-507):
 * its former tabs Tests, Schedules, and Rules are project tabs, and the
 * Artifacts tab is gone.
 */
export const PROJECT_TABS = [
  { path: "board", label: "Board" },
  // The test runs of the project (docs/api.md, test runs); `tests/:id` is the page of one run.
  { path: "tests", label: "Tests" },
  // The schedules and triggers of the project; the page keeps its own `?tab=triggers` sub-tab.
  { path: "schedules", label: "Schedules" },
  // The board rules of the project (docs/api.md §19.12): the rules that run, the lint, the runs, and the switch to rules.
  { path: "rules", label: "Rules" },
  { path: "dashboard", label: "Dashboard" },
  { path: "agents", label: "Agents" },
  { path: "repos", label: "Repos" },
  { path: "knowledge", label: "Knowledge" },
  { path: "pipelines", label: "Pipelines" },
  { path: "calendar", label: "Calendar" },
  { path: "logs", label: "Logs" },
  { path: "settings", label: "Settings" },
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
  // The project edit and delete, and the Targets panel. The repos have their own project tab.
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
  // The Inbox threads (TER-466): a forum table; the badge counts the threads that wait on you.
  { href: "/threads", label: "Threads" },
  // The logs screen (LOGS_TABS): mail, events, and audit. Active on every /logs/... tab.
  { href: "/logs", label: "Logs" },
  { href: "/schedules", label: "Schedules" },
  // The calendars of every project and the global calendar (docs/api.md §33).
  { href: "/calendar", label: "Calendar" },
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
  // The tool proxies (GitHub #21): definitions, grants, and recent runs.
  { path: "tools", label: "Tools" },
  { path: "targets", label: "Targets" },
  // The config repo: for the operator or a sysadmin.
  { path: "harness", label: "Harness config" },
  { path: "hosts", label: "Hosts" },
  // The link keys of other daemons that show this one (docs/api.md §36).
  { path: "links", label: "Links" },
  // The build of the daemon (docs/api.md §4) and the stale host agents. Self-update (#19) goes here.
  { path: "software", label: "Software" },
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
 * - The project Artifacts page was a tab of the board screen. The tab is
 *   gone (TER-507); the page stays at /p/:project/board/artifacts/:id? with
 *   no tab, so an old link still opens it.
 * - Version objects are gone (TER-58): a version is the card label
 *   `version:<name>`, and the Cards tab filters and groups by it
 *   (`?version=<name>&group=version`). Old mail and bookmarks can have
 *   `/p/<project>/versions/<id>` or a path of the old Versions tab, so
 *   these paths redirect in one hop to the board. The old version ID
 *   names no label, so it does not go along.
 * - Tests, Schedules, and Rules were tabs of the board screen
 *   (/p/:project/board/<tab>). They are project tabs now (TER-507), at
 *   /p/:project/tests[/:id], /p/:project/schedules, and /p/:project/rules.
 *   The daemon, old mail, and bookmarks link the board paths, and a schedule
 *   link has a query (`?open=...&tab=triggers`), so the board paths stay as
 *   redirects.
 * - The project Wiki and Memory pages are tabs of the knowledge screen
 *   (KNOWLEDGE_TABS). The wiki redirect keeps the page path (`*path`).
 *   `/p/:project/knowledge` opens the Wiki tab.
 * - The global Secrets, Templates, Skills, and Targets pages are tabs of the
 *   settings screen. A bookmark can have an old path, and a link can have a
 *   query (`/targets?project=...`), so the old paths stay as redirects. The
 *   skill redirect keeps the skill name.
 * - The Inbox threads have their own Threads screen (TER-466): a thread
 *   opens at `/threads/:id`. The Threads tab of the Inbox is gone: see
 *   {@link inboxRedirect}.
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
  { path: "/p/:project/versions/:id", to: "/p/:project/board" },
  { path: "/p/:project/board/versions", to: "/p/:project/board" },
  { path: "/p/:project/board/versions/:id", to: "/p/:project/board" },
  { path: "/p/:project/artifacts", to: "/p/:project/board/artifacts" },
  { path: "/p/:project/artifacts/:id", to: "/p/:project/board/artifacts/:id" },
  { path: "/p/:project/board/schedules", to: "/p/:project/schedules" },
  { path: "/p/:project/board/rules", to: "/p/:project/rules" },
  { path: "/p/:project/board/tests", to: "/p/:project/tests" },
  { path: "/p/:project/board/tests/:id", to: "/p/:project/tests/:id" },
  { path: "/p/:project/wiki/*path", to: "/p/:project/knowledge/wiki/*path" },
  { path: "/p/:project/memory", to: "/p/:project/knowledge/memory" },
  { path: "/p/:project/knowledge", to: "/p/:project/knowledge/wiki" },
  { path: "/secrets", to: "/settings/secrets" },
  { path: "/templates", to: "/settings/templates" },
  { path: "/skills", to: "/settings/skills" },
  { path: "/skills/:name", to: "/settings/skills/:name" },
  { path: "/targets", to: "/settings/targets" },
  { path: "/inbox/threads/:id", to: "/threads/:id" },
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

/**
 * The Threads tab of the Inbox moved to the Threads screen (TER-466): the
 * `/threads` target of an Inbox query with `tab=thread`, else undefined. The
 * project filter goes along. The old Done tab is the Archived filter; the old
 * Tracking tab is in Open.
 */
export function inboxRedirect(search: string): string | undefined {
  const q = new URLSearchParams(search.replace(/^\?/, ""));
  if (q.get("tab") !== "thread") return undefined;
  const next = new URLSearchParams();
  if (q.get("status") === "done") next.set("status", "archived");
  const project = q.get("project");
  if (project) next.set("project", project);
  const rest = next.toString();
  return rest ? `/threads?${rest}` : "/threads";
}
