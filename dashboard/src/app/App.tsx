import { Navigate, Route, Router, useParams } from "@solidjs/router";
import { For, lazy, Match, Show, Switch } from "solid-js";
import { isMock } from "../api";
import { auth } from "../stores";
import { Layout } from "./Layout";
import { MOVED_ROUTES, movedTo, withQuery } from "./nav";
import { LoginScreen } from "./LoginScreen";
import { Home } from "../features/home/Home";
import { AgentsPage } from "../features/agents/AgentsPage";
import { BoardPage } from "../features/board/BoardPage";
import BoardScreen from "../features/board/BoardScreen";

const AgentDetail = lazy(() => import("../features/agents/AgentDetail"));
const SubscriptionsPage = lazy(() => import("../features/subscriptions/SubscriptionsPage"));
const MailPage = lazy(() => import("../features/mail/MailPage"));
const InboxPage = lazy(() => import("../features/inbox/InboxPage"));
const MemoryPage = lazy(() => import("../features/memory/MemoryPage"));
const WikiPage = lazy(() => import("../features/wiki/WikiPage"));
const ArtifactsPage = lazy(() => import("../features/artifacts/ArtifactsPage"));
const TargetsPage = lazy(() => import("../features/targets/TargetsPage"));
const SchedulesPage = lazy(() => import("../features/schedules/SchedulesPage"));
const TemplatesPage = lazy(() => import("../features/templates/TemplatesPage"));
const SystemPromptsPage = lazy(() => import("../features/systemprompts/SystemPromptsPage"));
const HostsPage = lazy(() => import("../features/hosts/HostsPage"));
const AuditPage = lazy(() => import("../features/audit/AuditPage"));
const EventsPage = lazy(() => import("../features/events/EventsPage"));
const ProjectsPage = lazy(() => import("../features/projects/ProjectsPage"));
const ProjectSettings = lazy(() => import("../features/projects/ProjectSettings"));
const ReposPage = lazy(() => import("../features/repos/ReposPage"));
const RepoPage = lazy(() => import("../features/repos/RepoPage"));
const KnowledgePage = lazy(() => import("../features/knowledge/KnowledgePage"));
const DbTypesPage = lazy(() => import("../features/db/DbTypesPage"));
const DbRecordsPage = lazy(() => import("../features/db/DbRecordsPage"));
const LogsPage = lazy(() => import("../features/logs/LogsPage"));
const SettingsPage = lazy(() => import("../features/settings/SettingsPage"));
const AccountPage = lazy(() => import("../features/settings/AccountPage"));
const ConfigPage = lazy(() => import("../features/config/ConfigPage"));
const SkillsPage = lazy(() => import("../features/skills/SkillsPage"));
const SessionPage = lazy(() => import("../features/sessions/SessionPage"));
const SecretsPage = lazy(() => import("../features/secrets/SecretsPage"));
// Mock only: the hostile payloads, to eyeball that the sanitizer keeps them inert.
const XssPage = lazy(() => import("../features/devxss/XssPage"));

function ProjectIndex() {
  const params = useParams();
  return <Navigate href={`/p/${params.project}/board`} />;
}

function NotFound() {
  return (
    <div class="empty">
      Page not found. <a href="/">Go home</a>.
    </div>
  );
}

export function App() {
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
        <Route path="/agents/:id" component={AgentDetail} />
        <Route path="/sessions/:id" component={SessionPage} />
        <Route path="/inbox" component={InboxPage} />
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
          <Route path="/targets" component={TargetsPage} />
          <Route path="/harness" component={ConfigPage} />
          <Route path="/hosts" component={HostsPage} />
        </Route>
        <Route path="/p/:project" component={ProjectIndex} />
        <Route path="/p/:project/agents" component={AgentsPage} />
        {/* The board screen: the tabs Cards, Artifacts, and Schedules (nav.ts BOARD_TABS). The Cards tab is the index route: /p/:project/board; it manages the versions too (?rows=version&version=<id>). */}
        <Route path="/p/:project/board" component={BoardScreen}>
          <Route path="/" component={BoardPage} />
          <Route path="/artifacts/:id?" component={ArtifactsPage} />
          <Route path="/schedules" component={SchedulesPage} />
        </Route>
        {/* The repos of the project, and the browser of one repo (its trees: main and each agent checkout). */}
        <Route path="/p/:project/repos" component={ReposPage} />
        <Route path="/p/:project/repos/:repo" component={RepoPage} />
        {/* The project logs: the Mail and Events tabs (nav.ts PROJECT_LOGS_TABS), each for this project only. */}
        <Route path="/p/:project/logs" component={LogsPage}>
          <Route path="/mail" component={MailPage} />
          <Route path="/events" component={EventsPage} />
        </Route>
        {/* The project settings screen: the tabs General, Secrets, and Templates (nav.ts PROJECT_SETTINGS_TABS). /p/:project/settings redirects to the first tab. */}
        <Route path="/p/:project/settings" component={SettingsPage}>
          <Route path="/general" component={ProjectSettings} />
          <Route path="/secrets" component={SecretsPage} />
          <Route path="/templates" component={TemplatesPage} />
        </Route>
        {/* The knowledge screen: the tabs Wiki, Memory, and DB (nav.ts KNOWLEDGE_TABS). /p/:project/knowledge redirects to the Wiki tab. */}
        <Route path="/p/:project/knowledge" component={KnowledgePage}>
          <Route path="/wiki/*path" component={WikiPage} />
          <Route path="/memory" component={MemoryPage} />
          <Route path="/db" component={DbTypesPage} />
          <Route path="/db/:type" component={DbRecordsPage} />
        </Route>
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
