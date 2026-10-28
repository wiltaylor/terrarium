import { Navigate, Route, Router, useParams } from "@solidjs/router";
import { For, lazy, Match, Show, Switch } from "solid-js";
import { isMock } from "../api";
import { auth } from "../stores";
import { Layout } from "./Layout";
import { MOVED_ROUTES, movedTo } from "./nav";
import { LoginScreen } from "./LoginScreen";
import { Home } from "../features/home/Home";
import { AgentsPage } from "../features/agents/AgentsPage";
import { BoardPage } from "../features/board/BoardPage";

const AgentDetail = lazy(() => import("../features/agents/AgentDetail"));
const SubscriptionsPage = lazy(() => import("../features/subscriptions/SubscriptionsPage"));
const MailPage = lazy(() => import("../features/mail/MailPage"));
const InboxPage = lazy(() => import("../features/inbox/InboxPage"));
const MemoryPage = lazy(() => import("../features/memory/MemoryPage"));
const WikiPage = lazy(() => import("../features/wiki/WikiPage"));
const ArtifactsPage = lazy(() => import("../features/artifacts/ArtifactsPage"));
const VersionsPage = lazy(() => import("../features/versions/VersionsPage"));
const TargetsPage = lazy(() => import("../features/targets/TargetsPage"));
const SchedulesPage = lazy(() => import("../features/schedules/SchedulesPage"));
const TemplatesPage = lazy(() => import("../features/templates/TemplatesPage"));
const HostsPage = lazy(() => import("../features/hosts/HostsPage"));
const AuditPage = lazy(() => import("../features/audit/AuditPage"));
const EventsPage = lazy(() => import("../features/events/EventsPage"));
const ProjectsPage = lazy(() => import("../features/projects/ProjectsPage"));
const ProjectSettings = lazy(() => import("../features/projects/ProjectSettings"));
const SettingsPage = lazy(() => import("../features/settings/SettingsPage"));
const ConfigPage = lazy(() => import("../features/config/ConfigPage"));
const SkillsPage = lazy(() => import("../features/skills/SkillsPage"));
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
        <Route path="/agents" component={AgentsPage} />
        <Route path="/agents/:id" component={AgentDetail} />
        <Route path="/templates" component={TemplatesPage} />
        <Route path="/skills/:name?" component={SkillsPage} />
        <Route path="/subscriptions" component={SubscriptionsPage} />
        <Route path="/mail" component={MailPage} />
        <Route path="/inbox" component={InboxPage} />
        <Route path="/targets" component={TargetsPage} />
        <Route path="/schedules" component={SchedulesPage} />
        <Route path="/hosts" component={HostsPage} />
        <Route path="/events" component={EventsPage} />
        <Route path="/audit" component={AuditPage} />
        <Route path="/config" component={ConfigPage} />
        <Route path="/settings" component={SettingsPage} />
        <Route path="/p/:project" component={ProjectIndex} />
        <Route path="/p/:project/agents" component={AgentsPage} />
        <Route path="/p/:project/board" component={BoardPage} />
        <Route path="/p/:project/mail" component={MailPage} />
        <Route path="/p/:project/settings" component={ProjectSettings} />
        <Route path="/p/:project/memory" component={MemoryPage} />
        <Route path="/p/:project/wiki/*path" component={WikiPage} />
        <Route path="/p/:project/artifacts/:id?" component={ArtifactsPage} />
        <Route path="/p/:project/versions/:id?" component={VersionsPage} />
        <Route path="/p/:project/schedules" component={SchedulesPage} />
        <Show when={isMock}>
          <Route path="/dev/xss" component={XssPage} />
        </Show>
        <For each={MOVED_ROUTES}>{(r) => <Route path={r.path} component={() => <Navigate href={({ location }) => movedTo(location.pathname) ?? r.to} />} />}</For>
        <Route path="*" component={NotFound} />
      </Router>
      </Match>
    </Switch>
  );
}
