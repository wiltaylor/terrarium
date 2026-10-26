// The typed API surface of PLAN §17, coded to docs/api.md where the daemon
// has the endpoint. `createHttpApi` talks to the daemon. `createMockApi`
// (./mock) implements the same interface in memory.
//
// Lists: the daemon answers a JSON array and puts the next cursor in the
// `X-Next-Cursor` header (docs/api.md §2.4). Methods that return `Page<T>`
// expose the cursor for "load more". Methods that return `T[]` follow every
// cursor with `collectAll`.

import { ApiError, AuthError, seg, type Page, type Query, type Transport } from "./http";
import { collectAll, type PageQuery } from "../lib/paging";
import { createWsEventStream, type EventStream, type PtySocket } from "./sockets";
import { createTranscriptStream, type TranscriptStream } from "./sse";
import { onArtifactOrigin } from "../lib/artifact-origin";
import { withScope } from "../lib/target-scope";
import type {
  Agent,
  AgentDef,
  AgentEvent,
  AgentPatch,
  ApiKeyInput,
  Artifact,
  ArtifactJsonUpload,
  ArtifactLinkKind,
  ArtifactUrl,
  ArtifactUsage,
  AuditEntry,
  AuditQuery,
  Board,
  Card,
  CardLinkKind,
  CardPatch,
  CardQuery,
  ColumnEdit,
  CreatedMemory,
  DetectedLogin,
  EventQuery,
  ExecOpened,
  ExecRequest,
  HarnessReport,
  HarnessUpdateOptions,
  HarnessUpdateResult,
  Health,
  Host,
  InboxQuery,
  Lease,
  LeaseGrant,
  LeaseGranted,
  LoginSession,
  MailQuery,
  Memory,
  MemoryGraph,
  MemoryPatch,
  MemoryQuery,
  Message,
  NewCard,
  NewHostToken,
  NewMemory,
  NewMessage,
  NewProject,
  NewRepo,
  ModelList,
  NewSubscription,
  NewVersion,
  PageStatus,
  ProbeOutcome,
  Project,
  QuotaSample,
  Relation,
  Repo,
  Revision,
  Role,
  NewSchedule,
  EventSource,
  EventSourcePatch,
  EventSourceWithSecret,
  NewEventSource,
  Schedule,
  SchedulePatch,
  ScheduleQuery,
  ScheduleRun,
  Session,
  SessionGrant,
  StartLogin,
  Subscription,
  NewTarget,
  Target,
  TargetReport,
  TranscriptPage,
  TargetVerbBody,
  TargetSnapshotList,
  SnapshotQuery,
  SnapshotDeleted,
  UsageMe,
  CardUsage,
  UsagePoint,
  UsageQuery,
  VerifyResult,
  Version,
  VersionCompare,
  VersionPatch,
  VersionStatus,
  WikiDiff,
  WikiPage,
  WikiPageSummary,
} from "./types";

export interface Api {
  health(): Promise<Health>;
  /**
   * `GET /api/harnesses`: each harness against its pin and tested range. `refresh` runs the binaries
   * again (and asks npm again); `latest` adds the latest npm release and the update kind.
   */
  harnesses(refresh?: boolean, latest?: boolean): Promise<HarnessReport>;
  /** `POST /api/harnesses/{name}/update`: npm installs the version in the container (operator or sysadmin). */
  harnessUpdate(name: string, opts?: HarnessUpdateOptions): Promise<HarnessUpdateResult>;
  /** `POST /api/harnesses/update`: every harness and the extension to the latest. */
  harnessUpdateAll(opts?: { force?: boolean }): Promise<{ results: HarnessUpdateResult[] }>;

  /** Dashboard sessions (docs/api.md §1.1). */
  session: {
    /**
     * `POST /api/session` with `Authorization: Bearer <token>`. The daemon
     * sets the `HttpOnly` cookie and answers the CSRF value. Null: this
     * daemon has no sessions (`404` or `405`); keep to the bearer token.
     * A wrong token is an `AuthError`.
     */
    create(token: string): Promise<SessionGrant | null>;
    /** `DELETE /api/session`: the daemon clears the cookie. A daemon without sessions is not an error. */
    remove(): Promise<void>;
  };

  projects: {
    list(): Promise<Project[]>;
    get(name: string): Promise<Project>;
    create(p: NewProject): Promise<Project>;
    update(name: string, patch: { description?: string; card_prefix?: string }): Promise<Project>;
    /** Sends `?confirm=true`. The daemon refuses a project with agents or a file project. */
    remove(name: string): Promise<void>;
    agents(name: string): Promise<Agent[]>;
    targets(name: string): Promise<Target[]>;
    repos(name: string): Promise<Repo[]>;
    attachRepo(name: string, repo: NewRepo): Promise<Repo>;
  };

  repos: {
    /** Not in the daemon yet (docs/api.md §10): answers 404. */
    checkout(id: string, opts: { target?: string; branch?: string; path?: string }): Promise<void>;
  };

  agents: {
    list(project?: string): Promise<Agent[]>;
    /** `id` is a ULID, `project/name`, or a unique name. */
    get(id: string): Promise<Agent>;
    create(def: AgentDef): Promise<Agent>;
    update(id: string, patch: AgentPatch): Promise<Agent>;
    /** `confirm` adds `?confirm=true`, needed when the agent has a live session. */
    remove(id: string, confirm?: boolean): Promise<void>;
    /** 202: the body shows the agent when the daemon accepted the call. Watch the events for the change. */
    start(id: string): Promise<Agent>;
    stop(id: string): Promise<Agent>;
    restart(id: string): Promise<Agent>;
    send(id: string, text: string): Promise<{ accepted: boolean }>;
    answer(id: string, body: { request_id: string; answer: unknown } | { text: string }): Promise<{ accepted: boolean }>;
    events(id: string, q?: EventQuery): Promise<Page<AgentEvent>>;
    sessions(id: string, q?: PageQuery): Promise<Page<Session>>;
    /** `GET .../sessions/{session}/transcript?after=&limit=` (docs/api.md §6, transcripts): blocks with `seq > after`, oldest first. */
    transcript(id: string, session: string, q?: { after?: number; limit?: number }): Promise<TranscriptPage>;
    /** `GET .../sessions/{session}/transcript/stream?after=` (SSE): new and grown blocks, live. */
    transcriptStream(id: string, session: string, opts?: { after?: number }): TranscriptStream;
    roles(id: string): Promise<Role[]>;
    setRoles(id: string, roles: Role[]): Promise<Role[]>;
    /** `GET /api/agents/{id}/leases` (docs/api.md §17.7). `all` also lists the ended leases. */
    leases(id: string, opts?: { all?: boolean }): Promise<Lease[]>;
    /** Bidirectional PTY. See ./sockets for the protocol. */
    pty(id: string): PtySocket;
  };

  board: {
    get(project: string): Promise<Board>;
    update(project: string, columns: ColumnEdit[]): Promise<Board>;
  };

  cards: {
    list(project: string, q?: CardQuery): Promise<Page<Card>>;
    create(project: string, card: NewCard): Promise<Card>;
    get(id: string): Promise<Card>;
    update(id: string, patch: CardPatch): Promise<Card>;
    archive(id: string): Promise<void>;
    move(id: string, to: { column: string; position?: number; comment?: string }): Promise<Card>;
    assign(id: string, assignee: string | null): Promise<Card>;
    comment(id: string, body: string): Promise<Card>;
    link(id: string, kind: CardLinkKind, ref: string): Promise<Card>;
    events(id: string, q?: EventQuery): Promise<Page<AgentEvent>>;
  };

  /** docs/api.md §10. `id` is `MEM-n` or `project/MEM-n`; pass the `key` from the operator. */
  memories: {
    list(project: string, q?: MemoryQuery): Promise<Memory[]>;
    create(project: string, m: NewMemory): Promise<CreatedMemory>;
    get(id: string): Promise<Memory>;
    update(id: string, patch: MemoryPatch): Promise<Memory>;
    remove(id: string): Promise<void>;
    graph(id: string, depth?: number): Promise<MemoryGraph>;
    /** Answers the memory at the `from` end. */
    link(id: string, to: string, relation: Relation, note?: string): Promise<Memory>;
    unlink(id: string, to: string, relation: Relation): Promise<Memory>;
    /** Answers the superseded memory. */
    supersede(id: string, by: string): Promise<Memory>;
  };

  /** docs/api.md §11. `id` is the page ULID or `project/path`. */
  wiki: {
    tree(project: string, q?: { q?: string; status?: PageStatus; deleted?: boolean }): Promise<WikiPageSummary[]>;
    create(project: string, page: { path: string; body: string; summary: string; title?: string }): Promise<WikiPage>;
    get(id: string): Promise<WikiPage>;
    /** A stale `base` is `409 stale_base` with `details: {base, current, diff}`. */
    save(id: string, page: { body: string; base: number; summary: string; title?: string }): Promise<WikiPage>;
    remove(id: string): Promise<void>;
    section(id: string, s: { heading: string; body: string; summary: string; base?: number }): Promise<WikiPage>;
    append(id: string, s: { body: string; summary: string }): Promise<WikiPage>;
    revisions(id: string): Promise<Revision[]>;
    revision(id: string, rev: number): Promise<Revision & { body: string }>;
    diff(id: string, a?: number, b?: number): Promise<WikiDiff>;
    restore(id: string, rev: number): Promise<WikiPage>;
    move(id: string, path: string): Promise<WikiPage>;
    backlinks(id: string): Promise<WikiPageSummary[]>;
    review(id: string, decision: "approve" | "comment", comment?: string): Promise<WikiPage>;
  };

  /** docs/api.md §12. `id` is `ART-n`, `project/ART-n`, or the `uid`. */
  artifacts: {
    list(project: string): Promise<Artifact[]>;
    usage(project: string): Promise<ArtifactUsage>;
    /** Multipart: `file` (a tar for a bundle), `name`, `kind`, `attach_kind`, `attach_ref`. */
    upload(project: string, form: FormData): Promise<Artifact>;
    /** The JSON body with base64 data. */
    uploadJson(project: string, body: ArtifactJsonUpload): Promise<Artifact>;
    get(id: string): Promise<Artifact>;
    remove(id: string): Promise<void>;
    link(id: string, kind: ArtifactLinkKind, ref: string): Promise<Artifact>;
    /** A short-lived signed URL on the serving routes (`/a/{uid}/...`). */
    url(id: string, ttl?: number): Promise<ArtifactUrl>;
    /** Text content for text, Markdown, and code previews, from a signed URL. */
    text(url: string): Promise<string>;
  };

  /** docs/api.md §13. `id` is the ULID or `project/name`. */
  versions: {
    list(project: string, q?: { status?: VersionStatus; name?: string }): Promise<Version[]>;
    create(project: string, v: NewVersion): Promise<Version>;
    get(id: string): Promise<Version>;
    update(id: string, patch: VersionPatch): Promise<Version>;
    transition(id: string, action: "submit" | "approve" | "reject" | "release", comment?: string): Promise<Version>;
    compare(id: string, to: string): Promise<VersionCompare>;
  };

  /** docs/api.md §16. */
  subscriptions: {
    list(): Promise<Subscription[]>;
    get(name: string): Promise<Subscription>;
    /** `201`, `source = api`. `409 conflict` when the name exists; `422 invalid` for an unknown probe. */
    create(def: NewSubscription): Promise<Subscription>;
    /** `409 file_managed` for a file subscription. */
    remove(name: string): Promise<void>;
    /** Samples, oldest first. `since` defaults to 24 hours ago. */
    quota(name: string, q?: { since?: string; window?: string; limit?: number }): Promise<QuotaSample[]>;
    /** The models the subscription can use. Cached for 6 h by the daemon; `refresh` reads the list again. */
    models(name: string, q?: { refresh?: boolean }): Promise<ModelList>;
    /** A failed probe is still `200`, with `probe_status: failed`. */
    probe(name: string): Promise<ProbeOutcome>;
    verify(name: string): Promise<VerifyResult>;
    logout(name: string): Promise<Subscription>;
    /** Write-only. The answer never has the key. */
    setApiKey(name: string, input: ApiKeyInput): Promise<Subscription>;
    detect(): Promise<DetectedLogin[]>;
    startLogin(req: StartLogin): Promise<LoginSession>;
    loginState(id: string): Promise<LoginSession>;
    /** `202`; the session becomes `cancelled`. */
    cancelLogin(id: string): Promise<void>;
    /** `409 not_running` when the session ended. */
    loginInput(id: string, code: string): Promise<void>;
    /** The login terminal, with the PTY protocol of docs/api.md §6.5. */
    loginPty(id: string): PtySocket;
  };

  /** docs/api.md §16.3. */
  usage: {
    series(q: UsageQuery): Promise<UsagePoint[]>;
    /** `GET /api/usage/me?agent=`: the agent's subscription, its share today, and its holds. */
    me(agent: string): Promise<UsageMe>;
    /** `GET /api/cards/{id}/usage`: one card's usage by model and by agent. All time unless `since`/`until`. */
    card(id: string, q?: { since?: string; until?: string }): Promise<CardUsage>;
  };

  mail: {
    list(q?: MailQuery): Promise<Page<Message>>;
    get(id: string): Promise<Message>;
    send(m: NewMessage): Promise<Message>;
    markRead(id: string): Promise<Message>;
    /** One message for each participant except the sender. */
    reply(id: string, body: string, subject?: string): Promise<Message[]>;
  };

  inbox: {
    list(project: string, q?: InboxQuery): Promise<Page<Message>>;
    send(project: string, m: Omit<NewMessage, "to">): Promise<Message>;
    markRead(project: string, id: string): Promise<Message>;
    /** From `project:<name>`, one message for each participant. */
    reply(project: string, id: string, body: string, subject?: string): Promise<Message[]>;
  };

  audit(q?: AuditQuery): Promise<Page<AuditEntry>>;

  /** PLAN §17, §19, the daemon's `api/targets.rs`. */
  targets: {
    list(): Promise<Target[]>;
    /** `POST /api/targets`. No `project`: a global target, which needs a sysadmin (`403 forbidden`). */
    create(t: NewTarget): Promise<Target>;
    /** A lifecycle verb (docs/api.md §17.3). It runs to its end; the answer is the executor's report, not a target: refetch the list. */
    action(id: string, action: TargetAction, body?: TargetVerbBody): Promise<TargetReport>;
    /** Open an exec session. `{shell: true, pty}` gives an operator terminal. */
    exec(id: string, req: ExecRequest): Promise<ExecOpened>;
    /** The session terminal, with the PTY protocol of docs/api.md §6.5. */
    sessionPty(id: string, session: string): PtySocket;
    /** Kill the run. */
    closeSession(id: string, session: string): Promise<void>;
    /** `GET /api/targets/{name}/snapshots`: the snapshots the host reports, with `managed`; it drops the stored names the host lost. A docker target is `422 invalid`. */
    snapshots(name: string, q?: SnapshotQuery): Promise<TargetSnapshotList>;
    /** `DELETE /api/targets/{name}/snapshots/{snap}`. `terrarium-clean` with `snapshot_before_task` needs `force` (`409 conflict`). */
    deleteSnapshot(name: string, snap: string, q?: SnapshotQuery): Promise<SnapshotDeleted>;
    /** `GET /api/targets/{name}/leases`, newest first. `all` also lists the ended leases (the last 100). */
    leases(name: string, opts?: { all?: boolean }): Promise<Lease[]>;
    /** `POST /api/targets/{name}/leases`. A clash is `409 lease_held` with the holder in `details`. */
    grant(name: string, body: LeaseGrant): Promise<LeaseGranted>;
    /** `PATCH /api/targets/{name}/leases/{agent}`: a new `until`; `null` is until a revoke. */
    extendLease(name: string, agent: string, until: string | null): Promise<Lease>;
    /** `DELETE /api/targets/{name}/leases/{agent}`: answers the ended lease. */
    revokeLease(name: string, agent: string): Promise<Lease>;
  };

  /**
   * docs/api.md §19. Rights: the operator always; a project_admin within its
   * project; a member reads only; a global schedule needs a sysadmin
   * (`403 forbidden`). A bad field is `422 invalid` with `details.field`.
   */
  schedules: {
    /** `GET /api/schedules?project=&enabled=`: with `project`, the global schedules and the project's. */
    list(q?: ScheduleQuery): Promise<Schedule[]>;
    get(id: string): Promise<Schedule>;
    create(s: NewSchedule): Promise<Schedule>;
    update(id: string, patch: SchedulePatch): Promise<Schedule>;
    remove(id: string): Promise<void>;
    /** `POST /api/schedules/{id}/run`: run now. The run ends later; watch the `schedule` events. */
    run(id: string): Promise<ScheduleRun>;
    enable(id: string): Promise<Schedule>;
    disable(id: string): Promise<Schedule>;
    /** `GET /api/schedules/{id}/runs?limit=&cursor=`, newest first. */
    runs(id: string, q?: PageQuery): Promise<Page<ScheduleRun>>;
  };

  /**
   * docs/api.md §20. Rights: the operator always; a project_admin within its
   * project; a member reads only; a global source needs a sysadmin. The
   * secret is only in the answers of `create` and `rotate`.
   */
  eventSources: {
    /** `GET /api/event-sources?project=`: with `project`, the project's sources and the global ones when the caller may read them. */
    list(q?: { project?: string }): Promise<EventSource[]>;
    get(id: string): Promise<EventSource>;
    create(s: NewEventSource): Promise<EventSourceWithSecret>;
    update(id: string, patch: EventSourcePatch): Promise<EventSource>;
    /** A new secret; the old one stops at once. */
    rotate(id: string): Promise<EventSourceWithSecret>;
    remove(id: string): Promise<void>;
  };

  /** docs/host-protocol.md, the daemon's `api/hosts.rs`. */
  hosts: {
    list(): Promise<Host[]>;
    /** Mint a host and its token. The token shows once, in this answer only. */
    create(name: string): Promise<NewHostToken>;
    /** Revoke the token: the daemon sends `goodbye token_revoked` and closes the link. */
    revoke(name: string): Promise<void>;
  };

  events: {
    /** The firehose socket (`/api/events`), reconnecting with `?since=<last seq>`. */
    stream(opts?: { since?: number }): EventStream;
    /** One page of the log after `since`, oldest first. */
    history(q?: EventQuery): Promise<Page<AgentEvent>>;
    /**
     * The newest `seq` in the log (`GET /api/events/latest`), or null when
     * this daemon does not have the endpoint. Then the stream asks for the
     * whole log, as before.
     */
    latest(): Promise<number | null>;
  };
}

export type TargetAction = "up" | "down" | "snapshot" | "restore" | "status";

/** Read the answer of `GET /api/events/latest`: a number, `{seq}`, `{latest}`, or an event. */
export function parseLatest(data: unknown): number | null {
  if (typeof data === "number" && Number.isFinite(data)) return data;
  if (typeof data === "object" && data !== null) {
    const o = data as Record<string, unknown>;
    for (const k of ["seq", "latest", "last_seq"]) if (typeof o[k] === "number") return o[k];
  }
  return null;
}

export interface HttpApiOptions {
  /**
   * The artifact origin, for example `https://artifacts.example.com`
   * (`VITE_ARTIFACT_ORIGIN`). A signed URL that the daemon answers as a path
   * is put on this origin, so that previews never load from the dashboard
   * origin (docs/security-review.md F5).
   */
  artifactOrigin?: string;
}

export function createHttpApi(t: Transport, opts: HttpApiOptions = {}): Api {
  const get = <T>(path: string, query?: Query) => t.request<T>("GET", path, { query });
  const post = <T>(path: string, body?: unknown) => t.request<T>("POST", path, { body });
  const patch = <T>(path: string, body: unknown) => t.request<T>("PATCH", path, { body });
  const put = <T>(path: string, body: unknown) => t.request<T>("PUT", path, { body });
  const del = (path: string, query?: Query) => t.request<void>("DELETE", path, { query });
  const page = <T>(path: string, query?: Query) => t.page<T>(path, query);
  const all = <T>(path: string, query: Query = {}) => collectAll<T>((cursor) => t.page<T>(path, { ...query, cursor, limit: 1000 }));
  const socket = (path: string): PtySocket => {
    const ws = t.openSocket(t.wsUrl(path));
    ws.binaryType = "arraybuffer";
    return ws;
  };
  const p = (name: string) => `/api/projects/${seg(name)}`;
  const a = (id: string) => `/api/agents/${seg(id)}`;
  const c = (id: string) => `/api/cards/${seg(id)}`;
  const sub = (name: string) => `/api/subscriptions/${seg(name)}`;
  const login = (id: string) => `/api/subscriptions/login/${seg(id)}`;

  return {
    health: () => get("/api/health"),
    harnesses: (refresh, latest) => {
      const q: Query = {};
      if (refresh) q.refresh = true;
      if (latest) q.latest = true;
      return get("/api/harnesses", Object.keys(q).length ? q : undefined);
    },
    harnessUpdate: (name, opts = {}) => post(`/api/harnesses/${seg(name)}/update`, { version: opts.version ?? "latest", ...(opts.force ? { force: true } : {}) }),
    harnessUpdateAll: (opts = {}) => post("/api/harnesses/update", { version: "latest", ...(opts.force ? { force: true } : {}) }),

    session: {
      create: async (token) => {
        try {
          return await t.request<SessionGrant>("POST", "/api/session", { bearer: token, quiet401: true });
        } catch (e) {
          if (e instanceof ApiError && !(e instanceof AuthError) && (e.status === 404 || e.status === 405)) return null;
          throw e;
        }
      },
      remove: async () => {
        try {
          await t.request<void>("DELETE", "/api/session", { quiet401: true });
        } catch (e) {
          // No sessions on this daemon, or the session ended already.
          if (e instanceof ApiError && [401, 404, 405].includes(e.status)) return;
          throw e;
        }
      },
    },

    projects: {
      list: () => all("/api/projects"),
      get: (name) => get(p(name)),
      create: (proj) => post("/api/projects", proj),
      update: (name, body) => patch(p(name), body),
      remove: (name) => del(p(name), { confirm: true }),
      agents: (name) => all(`${p(name)}/agents`),
      targets: async (name) => (await all<Target>(`${p(name)}/targets`)).map(withScope),
      repos: (name) => all(`${p(name)}/repos`),
      attachRepo: (name, repo) => post(`${p(name)}/repos`, repo),
    },

    repos: {
      checkout: (id, opts) => post(`/api/repos/${seg(id)}/checkout`, opts),
    },

    agents: {
      list: (project) => all("/api/agents", { project }),
      get: (id) => get(a(id)),
      create: (def) => post("/api/agents", def),
      update: (id, body) => patch(a(id), body),
      remove: (id, confirm) => del(a(id), confirm ? { confirm: true } : undefined),
      start: (id) => post(`${a(id)}/start`),
      stop: (id) => post(`${a(id)}/stop`),
      restart: (id) => post(`${a(id)}/restart`),
      send: (id, text) => post(`${a(id)}/send`, { text }),
      answer: (id, body) => post(`${a(id)}/answer`, body),
      events: (id, q = {}) => page(`${a(id)}/events`, { ...q }),
      sessions: (id, q = {}) => page(`${a(id)}/sessions`, { ...q }),
      transcript: (id, session, q = {}) => get(`${a(id)}/sessions/${seg(session)}/transcript`, { after: q.after, limit: q.limit }),
      transcriptStream: (id, session, o = {}) =>
        createTranscriptStream((after, signal) => t.stream(`${a(id)}/sessions/${seg(session)}/transcript/stream`, { after }, signal), { after: o.after }),
      roles: (id) => get(`${a(id)}/roles`),
      setRoles: (id, roles) => put(`${a(id)}/roles`, roles),
      leases: (id, o = {}) => get(`${a(id)}/leases`, o.all ? { all: true } : undefined),
      pty: (id) => socket(`${a(id)}/pty`),
    },

    board: {
      get: (project) => get(`${p(project)}/board`),
      update: (project, columns) => patch(`${p(project)}/board`, { columns }),
    },

    cards: {
      list: (project, q = {}) => page(`${p(project)}/cards`, { ...q }),
      create: (project, card) => post(`${p(project)}/cards`, card),
      get: (id) => get(c(id)),
      update: (id, body) => patch(c(id), body),
      archive: (id) => del(c(id)),
      move: (id, to) => post(`${c(id)}/move`, to),
      assign: (id, assignee) => post(`${c(id)}/assign`, { assignee }),
      comment: (id, body) => post(`${c(id)}/comments`, { body }),
      link: (id, kind, ref) => post(`${c(id)}/links`, { kind, ref }),
      events: (id, q = {}) => page(`${c(id)}/events`, { ...q }),
    },

    memories: {
      list: (project, q = {}) => all(`${p(project)}/memories`, { ...q }),
      create: (project, m) => post(`${p(project)}/memories`, m),
      get: (id) => get(`/api/memories/${seg(id)}`),
      update: (id, body) => patch(`/api/memories/${seg(id)}`, body),
      remove: (id) => del(`/api/memories/${seg(id)}`),
      graph: (id, depth = 1) => get(`/api/memories/${seg(id)}/graph`, { depth }),
      link: (id, to, relation, note) => post(`/api/memories/${seg(id)}/links`, { to, relation, note }),
      unlink: (id, to, relation) => t.request("DELETE", `/api/memories/${seg(id)}/links`, { query: { to, relation } }),
      supersede: (id, by) => post(`/api/memories/${seg(id)}/supersede`, { by }),
    },

    wiki: {
      tree: (project, q = {}) => all(`${p(project)}/wiki`, { ...q }),
      create: (project, pg) => post(`${p(project)}/wiki`, pg),
      get: (id) => get(`/api/wiki/${seg(id)}`),
      save: (id, pg) => put(`/api/wiki/${seg(id)}`, pg),
      remove: (id) => del(`/api/wiki/${seg(id)}`),
      section: (id, s) => post(`/api/wiki/${seg(id)}/section`, s),
      append: (id, s) => post(`/api/wiki/${seg(id)}/append`, s),
      revisions: (id) => all(`/api/wiki/${seg(id)}/revisions`),
      revision: (id, rev) => get(`/api/wiki/${seg(id)}/revisions/${rev}`),
      diff: (id, x, y) => get(`/api/wiki/${seg(id)}/diff`, { a: x, b: y }),
      restore: (id, rev) => post(`/api/wiki/${seg(id)}/restore`, { rev }),
      move: (id, path) => post(`/api/wiki/${seg(id)}/move`, { path }),
      backlinks: (id) => all(`/api/wiki/${seg(id)}/backlinks`),
      review: (id, decision, comment) => post(`/api/wiki/${seg(id)}/review`, { decision, comment }),
    },

    artifacts: {
      list: (project) => all(`${p(project)}/artifacts`),
      usage: (project) => get(`${p(project)}/artifacts/usage`),
      upload: (project, form) => t.request("POST", `${p(project)}/artifacts`, { form }),
      uploadJson: (project, body) => post(`${p(project)}/artifacts`, body),
      get: (id) => get(`/api/artifacts/${seg(id)}`),
      remove: (id) => del(`/api/artifacts/${seg(id)}`),
      link: (id, kind, ref) => post(`/api/artifacts/${seg(id)}/links`, { kind, ref }),
      url: async (id, ttl) => {
        const signed = await post<ArtifactUrl>(`/api/artifacts/${seg(id)}/url`, ttl ? { ttl } : undefined);
        return { ...signed, url: onArtifactOrigin(signed.url, opts.artifactOrigin) };
      },
      text: async (url) => {
        // The artifact origin is another origin. No cookie, no referrer.
        const res = await fetch(url, { mode: "cors", credentials: "omit", referrerPolicy: "no-referrer" });
        if (!res.ok) throw new ApiError(res.status, `the artifact origin answered ${res.status}`);
        return res.text();
      },
    },

    versions: {
      list: (project, q = {}) => all(`${p(project)}/versions`, { ...q }),
      create: (project, v) => post(`${p(project)}/versions`, v),
      get: (id) => get(`/api/versions/${seg(id)}`),
      update: (id, body) => patch(`/api/versions/${seg(id)}`, body),
      transition: (id, action, comment) => post(`/api/versions/${seg(id)}/${action}`, comment ? { comment } : undefined),
      compare: (id, to) => get(`/api/versions/${seg(id)}/compare`, { to }),
    },

    subscriptions: {
      list: () => get("/api/subscriptions"),
      get: (name) => get(sub(name)),
      create: (def) => post("/api/subscriptions", def),
      remove: (name) => del(sub(name)),
      quota: (name, q = {}) => get(`${sub(name)}/quota`, { ...q }),
      models: (name, q = {}) => get(`${sub(name)}/models`, q.refresh ? { refresh: true } : {}),
      probe: (name) => post(`${sub(name)}/probe`),
      verify: (name) => post(`${sub(name)}/verify`),
      logout: (name) => post(`${sub(name)}/logout`),
      setApiKey: (name, input) => post(`${sub(name)}/api-key`, input),
      detect: () => get("/api/subscriptions/detect"),
      startLogin: (req) => post("/api/subscriptions/login", req),
      loginState: (id) => get(login(id)),
      cancelLogin: (id) => del(login(id)),
      loginInput: (id, code) => post(`${login(id)}/input`, { code }),
      loginPty: (id) => socket(`${login(id)}/pty`),
    },

    usage: {
      series: (q) => get("/api/usage", { ...q }),
      me: (agent) => get("/api/usage/me", { agent }),
      card: (id, q = {}) => get(`${c(id)}/usage`, { ...q }),
    },

    mail: {
      list: (q = {}) => page("/api/mail", { ...q }),
      get: (id) => get(`/api/mail/${seg(id)}`),
      send: (m) => post("/api/mail", m),
      markRead: (id) => post(`/api/mail/${seg(id)}/read`),
      reply: (id, body, subject) => post(`/api/mail/${seg(id)}/reply`, { body, subject }),
    },

    inbox: {
      list: (project, q = {}) => page(`${p(project)}/inbox`, { ...q }),
      send: (project, m) => post(`${p(project)}/inbox`, m),
      markRead: (project, id) => post(`${p(project)}/inbox/${seg(id)}/read`),
      reply: (project, id, body, subject) => post(`${p(project)}/inbox/${seg(id)}/reply`, { body, subject }),
    },

    audit: (q = {}) => page("/api/audit", { ...q }),

    targets: {
      list: async () => (await all<Target>("/api/targets")).map(withScope),
      create: async (target) => withScope(await post<Target>("/api/targets", target)),
      action: (id, action, body) => post(`/api/targets/${seg(id)}/${action}`, body),
      exec: (id, req) => post(`/api/targets/${seg(id)}/exec`, req),
      sessionPty: (id, session) => socket(`/api/targets/${seg(id)}/sessions/${seg(session)}/ws`),
      closeSession: (id, session) => del(`/api/targets/${seg(id)}/sessions/${seg(session)}`),
      snapshots: (name, q = {}) => get(`/api/targets/${seg(name)}/snapshots`, { ...q }),
      deleteSnapshot: (name, snap, q = {}) => t.request("DELETE", `/api/targets/${seg(name)}/snapshots/${seg(snap)}`, { query: { ...q } }),
      // A plain array, no cursor (docs/api.md §17.7).
      leases: (name, o = {}) => get(`/api/targets/${seg(name)}/leases`, o.all ? { all: true } : undefined),
      grant: (name, body) => post(`/api/targets/${seg(name)}/leases`, body),
      extendLease: (name, agent, until) => patch(`/api/targets/${seg(name)}/leases/${seg(agent)}`, { until }),
      revokeLease: (name, agent) => t.request("DELETE", `/api/targets/${seg(name)}/leases/${seg(agent)}`),
    },

    schedules: {
      list: (q = {}) => all("/api/schedules", { ...q }),
      get: (id) => get(`/api/schedules/${seg(id)}`),
      create: (body) => post("/api/schedules", body),
      update: (id, body) => patch(`/api/schedules/${seg(id)}`, body),
      remove: (id) => del(`/api/schedules/${seg(id)}`),
      run: (id) => post(`/api/schedules/${seg(id)}/run`),
      enable: (id) => post(`/api/schedules/${seg(id)}/enable`),
      disable: (id) => post(`/api/schedules/${seg(id)}/disable`),
      runs: (id, q = {}) => page(`/api/schedules/${seg(id)}/runs`, { ...q }),
    },

    eventSources: {
      list: (q = {}) => get("/api/event-sources", { ...q }),
      get: (id) => get(`/api/event-sources/${seg(id)}`),
      create: (body) => post("/api/event-sources", body),
      update: (id, body) => patch(`/api/event-sources/${seg(id)}`, body),
      rotate: (id) => post(`/api/event-sources/${seg(id)}/rotate`),
      remove: (id) => del(`/api/event-sources/${seg(id)}`),
    },

    hosts: {
      list: () => all("/api/hosts"),
      create: (name) => post("/api/hosts", { name }),
      revoke: (name) => del(`/api/hosts/${seg(name)}`),
    },

    events: {
      stream: (o = {}) =>
        createWsEventStream((since) => t.wsUrl("/api/events", { since }), { makeSocket: (u) => t.openSocket(u), since: o.since }),
      history: (q = {}) => page("/api/events", { ...q }),
      latest: async () => {
        try {
          return parseLatest(await get<unknown>("/api/events/latest"));
        } catch (e) {
          if (e instanceof AuthError) throw e;
          return null; // not on this daemon: fall back to the whole log
        }
      },
    },
  };
}
