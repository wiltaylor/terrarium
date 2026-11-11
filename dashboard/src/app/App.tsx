import { InstanceAnchor } from "../instance/navigation";
import { useInstance } from "../instance";
import { Route, Router, useParams } from "@solidjs/router";
import { Navigate } from "../instance/navigation";
import { For, lazy, Match, Show, Switch } from "solid-js";
import { isMock } from "../api";
import { escapeMalformedPath } from "../lib/url";

import { Layout } from "./Layout";
import { MOVED_ROUTES, movedTo, withQuery } from "./nav";
import { LoginScreen } from "./LoginScreen";
import { Home } from "../features/home/Home";
import { AgentsPage } from "../features/agents/AgentsPage";
import { BoardPage, DoneCardsPage } from "../features/board/BoardPage";

const AgentDetail = lazy(() => import("../features/agents/AgentDetail"));
const SubscriptionsPage = lazy(() => import("../features/subscriptions/SubscriptionsPage"));
const MailPage = lazy(() => import("../features/mail/MailPage"));
const InboxPage = lazy(() => import("../features/inbox/InboxPage"));
const ThreadPage = lazy(() => import("../features/inbox/ThreadPage"));
const ThreadsPage = lazy(() => import("../features/inbox/ThreadsPage"));
const MemoryPage = lazy(() => import("../features/memory/MemoryPage"));
const WikiPage = lazy(() => import("../features/wiki/WikiPage"));
const ArtifactsPage = lazy(() => import("../features/artifacts/ArtifactsPage"));
const TestRunsPage = lazy(() => import("../features/tests/TestRunsPage"));
const TestRunPage = lazy(() => import("../features/tests/TestRunPage"));
const DashboardPage = lazy(() => import("../features/dashboard/DashboardPage"));
const PipelinesPage = lazy(() => import("../features/pipelines/PipelinesPage"));
const CalendarPage = lazy(() => import("../features/calendar/CalendarPage"));
const TargetsPage = lazy(() => import("../features/targets/TargetsPage"));
const SchedulesPage = lazy(() => import("../features/schedules/SchedulesPage"));
const RulesPage = lazy(() => import("../features/rules/RulesPage"));
const TemplatesPage = lazy(() => import("../features/templates/TemplatesPage"));
const SystemPromptsPage = lazy(() => import("../features/systemprompts/SystemPromptsPage"));
const HostsPage = lazy(() => import("../features/hosts/HostsPage"));
const LinksPage = lazy(() => import("../features/links/LinksPage"));
const AuditPage = lazy(() => import("../features/audit/AuditPage"));
const EventsPage = lazy(() => import("../features/events/EventsPage"));
const ProjectsPage = lazy(() => import("../features/projects/ProjectsPage"));
const ProjectSettings = lazy(() => import("../features/projects/ProjectSettings"));
const ProjectScreen = lazy(() => import("../features/projects/ProjectScreen"));
const ReposPage = lazy(() => import("../features/repos/ReposPage"));
const RepoPage = lazy(() => import("../features/repos/RepoPage"));
const KnowledgePage = lazy(() => import("../features/knowledge/KnowledgePage"));
const DbTypesPage = lazy(() => import("../features/db/DbTypesPage"));
const DbRecordsPage = lazy(() => import("../features/db/DbRecordsPage"));
const LogsPage = lazy(() => import("../features/logs/LogsPage"));
const ManageBookmarks = lazy(() => import("./ManageBookmarks"));
const SettingsPage = lazy(() => import("../features/settings/SettingsPage"));
const AccountPage = lazy(() => import("../features/settings/AccountPage"));
const SoftwarePage = lazy(() => import("../features/settings/SoftwarePage"));
const ConfigPage = lazy(() => import("../features/config/ConfigPage"));
const SkillsPage = lazy(() => import("../features/skills/SkillsPage"));
const ToolsPage = lazy(() => import("../features/tools/ToolsPage"));
const SessionPage = lazy(() => import("../features/sessions/SessionPage"));
const SecretsPage = lazy(() => import("../features/secrets/SecretsPage"));
// Mock only: the hostile payloads, to eyeball that the sanitizer keeps them inert.
const XssPage = lazy(() => import("../features/devxss/XssPage"));

/** The project index opens Board, the first tab of the project shell. */
function ProjectIndex() {
  const params = useParams();
  return <Navigate href={`/p/${params.project}/board`} />;
}

function NotFound() {
  return (
    <div class="empty">
      Page not found. <InstanceAnchor href="/">Go home</InstanceAnchor>.
    </div>
  );
}

export function App() {
  const { auth } = useInstance();
  // @solidjs/router decodes the location in every link. Escape a malformed
  // path before it renders; valid paths and their project params stay intact.
  const pathname = escapeMalformedPath(window.location.pathname);
  if (pathname !== window.location.pathname) {
    window.history.replaceState(window.history.state, "", `${pathname}${window.location.search}${window.location.hash}`);
  }
  return (
    // At load: wait for `GET /api/session` (the cookie is the cache).
    // Signed out: the login screen alone. A session that ended keeps the
    // router mounted; the layout shows the login screen over the page.
    <Switch>
      <Match when={auth.mode() === "checking"}>
        <div class="empty" role="status">
          Loading…
        </div>
      </Match>
      <Match when={auth.mode() === "none"}>
        <LoginScreen />
      </Match>
      <Match when={true}>
      <Router root={Layout}>
        <Route path="/" component={Home} />
        <Route path="/projects" component={ProjectsPage} />
        <Route path="/bookmarks" component={ManageBookmarks} />
        <Route path="/agents/:id" component={AgentDetail} />
        <Route path="/sessions/:id" component={SessionPage} />
        <Route path="/inbox" component={InboxPage} />
        {/* The Threads screen (TER-466); `/inbox/threads/:id` redirects to `/threads/:id` (nav.ts MOVED_ROUTES). */}
        <Route path="/threads" component={ThreadsPage} />
        <Route path="/threads/:id" component={ThreadPage} />
        <Route path="/schedules" component={SchedulesPage} />
        {/* The logs screen: a tab bar over one route per tab (nav.ts LOGS_TABS). /logs redirects to the first tab. */}
        <Route path="/logs" component={LogsPage}>
          <Route path="/mail" component={MailPage} />
          <Route path="/events" component={EventsPage} />
          <Route path="/audit" component={AuditPage} />
        </Route>
        {/* The settings screen: a tab bar over one route per tab (nav.ts SETTINGS_TABS). /settings redirects to the first tab. */}
        <Route path="/settings" component={SettingsPage}>
          <Route path="/account" component={AccountPage} />
          <Route path="/subscriptions" component={SubscriptionsPage} />
          <Route path="/secrets" component={SecretsPage} />
          <Route path="/templates" component={TemplatesPage} />
          <Route path="/system-prompts" component={SystemPromptsPage} />
          <Route path="/skills/:name?" component={SkillsPage} />
          <Route path="/tools" component={ToolsPage} />
          <Route path="/targets" component={TargetsPage} />
          <Route path="/harness" component={ConfigPage} />
          <Route path="/hosts" component={HostsPage} />
          <Route path="/links" component={LinksPage} />
          <Route path="/software" component={SoftwarePage} />
        </Route>
        {/* Every project page keeps its URL under one shell with route tabs (nav.ts PROJECT_TABS). */}
        <Route path="/p/:project" component={ProjectScreen}>
          <Route path="/" component={ProjectIndex} />
          <Route path="/dashboard" component={DashboardPage} />
          <Route path="/pipelines" component={PipelinesPage} />
          <Route path="/calendar" component={CalendarPage} />
          <Route path="/agents" component={AgentsPage} />
          {/* The board: one route with no tab row of its own (TER-507). It filters and groups the cards by version label (?version=<name>&group=version). */}
          <Route path="/board" component={BoardPage} />
          <Route path="/board/done" component={DoneCardsPage} />
          {/* The Artifacts page has no tab (TER-507); old links to it still open it. */}
          <Route path="/board/artifacts/:id?" component={ArtifactsPage} />
          {/* Former board tabs, now project tabs (nav.ts PROJECT_TABS); the old /board/<tab> paths redirect (MOVED_ROUTES). */}
          <Route path="/tests" component={TestRunsPage} />
          <Route path="/tests/:id" component={TestRunPage} />
          <Route path="/schedules" component={SchedulesPage} />
          <Route path="/rules" component={RulesPage} />
          {/* The repos of the project, and the browser of one repo (its trees: main and each agent checkout). */}
          <Route path="/repos" component={ReposPage} />
          <Route path="/repos/:repo" component={RepoPage} />
          {/* The project logs: the Mail and Events tabs (nav.ts PROJECT_LOGS_TABS), each for this project only. */}
          <Route path="/logs" component={LogsPage}>
            <Route path="/mail" component={MailPage} />
            <Route path="/events" component={EventsPage} />
          </Route>
          {/* The project settings screen: the tabs General, Secrets, and Templates (nav.ts PROJECT_SETTINGS_TABS). /p/:project/settings redirects to the first tab. */}
          <Route path="/settings" component={SettingsPage}>
            <Route path="/general" component={ProjectSettings} />
            <Route path="/secrets" component={SecretsPage} />
            <Route path="/templates" component={TemplatesPage} />
          </Route>
          {/* The knowledge screen: the tabs Wiki, Memory, and DB (nav.ts KNOWLEDGE_TABS). /p/:project/knowledge redirects to the Wiki tab. */}
          <Route path="/knowledge" component={KnowledgePage}>
            <Route path="/wiki/*path" component={WikiPage} />
            <Route path="/memory" component={MemoryPage} />
            <Route path="/db" component={DbTypesPage} />
            <Route path="/db/:type" component={DbRecordsPage} />
          </Route>
        </Route>
        <Route path="/calendar" component={CalendarPage} />
        <Show when={isMock}>
          <Route path="/dev/xss" component={XssPage} />
        </Show>
        {/* A redirect keeps the query and the hash: an old `/subscriptions?add=api_key` link still opens the wizard. */}
        <For each={MOVED_ROUTES}>{(r) => <Route path={r.path} component={() => <Navigate href={({ location }) => `${withQuery(movedTo(location.pathname) ?? r.to, location.search)}${location.hash}`} />} />}</For>
        <Route path="*" component={NotFound} />
      </Router>
      </Match>
    </Switch>
  );
}
