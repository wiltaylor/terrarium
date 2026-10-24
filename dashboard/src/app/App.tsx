import { Navigate, Route, Router, useParams } from "@solidjs/router";
import { lazy, Show } from "solid-js";
import { isMock } from "../api";
import { auth } from "../stores";
import { Layout } from "./Layout";
import { TokenScreen } from "./TokenScreen";
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
const HostsPage = lazy(() => import("../features/hosts/HostsPage"));
const AuditPage = lazy(() => import("../features/audit/AuditPage"));
const EventsPage = lazy(() => import("../features/events/EventsPage"));
const ProjectsPage = lazy(() => import("../features/projects/ProjectsPage"));
const ProjectSettings = lazy(() => import("../features/projects/ProjectSettings"));
const SpendPage = lazy(() => import("../features/spend/SpendPage"));
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
    <Show when={isMock || !auth.needsToken()} fallback={<TokenScreen />}>
      <Router root={Layout}>
        <Route path="/" component={Home} />
        <Route path="/projects" component={ProjectsPage} />
        <Route path="/agents" component={AgentsPage} />
        <Route path="/agents/:id" component={AgentDetail} />
        <Route path="/subscriptions" component={SubscriptionsPage} />
        <Route path="/mail" component={MailPage} />
        <Route path="/targets" component={TargetsPage} />
        <Route path="/hosts" component={HostsPage} />
        <Route path="/events" component={EventsPage} />
        <Route path="/audit" component={AuditPage} />
        <Route path="/p/:project" component={ProjectIndex} />
        <Route path="/p/:project/board" component={BoardPage} />
        <Route path="/p/:project/inbox" component={InboxPage} />
        <Route path="/p/:project/settings" component={ProjectSettings} />
        <Route path="/p/:project/spend" component={SpendPage} />
        <Route path="/p/:project/memory" component={MemoryPage} />
        <Route path="/p/:project/wiki/*path" component={WikiPage} />
        <Route path="/p/:project/artifacts/:id?" component={ArtifactsPage} />
        <Route path="/p/:project/versions/:id?" component={VersionsPage} />
        <Show when={isMock}>
          <Route path="/dev/xss" component={XssPage} />
        </Show>
        <Route path="*" component={NotFound} />
      </Router>
    </Show>
  );
}
