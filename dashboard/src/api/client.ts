// The typed API surface of PLAN §17. `createHttpApi` talks to the daemon.
// `createMockApi` (./mock) implements the same interface in memory.

import { seg, type Query, type Transport } from "./http";
import { createWsEventStream, type EventStream, type PtySocket } from "./sockets";
import type {
  Agent,
  AgentEvent,
  ApiKeyInput,
  Artifact,
  AuditEntry,
  Board,
  Card,
  CardPatch,
  Column,
  DetectedLogin,
  Health,
  Host,
  LoginKind,
  LoginSession,
  Memory,
  MemoryGraph,
  MemoryLink,
  MemoryQuery,
  Message,
  MessageState,
  NewCard,
  NewMessage,
  Project,
  QuotaWindow,
  Relation,
  Repo,
  Revision,
  Role,
  Session,
  Subscription,
  Target,
  UsageBucket,
  UsageQuery,
  Version,
  WikiPage,
  WikiPageSummary,
} from "./types";

export interface Api {
  health(): Promise<Health>;

  projects: {
    list(): Promise<Project[]>;
    get(name: string): Promise<Project>;
    create(p: Pick<Project, "name" | "description">): Promise<Project>;
    update(name: string, patch: Partial<Pick<Project, "description">>): Promise<Project>;
    remove(name: string): Promise<void>;
    agents(name: string): Promise<Agent[]>;
    targets(name: string): Promise<Target[]>;
    repos(name: string): Promise<Repo[]>;
    attachRepo(name: string, repo: Pick<Repo, "url" | "default_branch" | "path">): Promise<Repo>;
  };

  repos: {
    checkout(id: string, opts: { target?: string; branch?: string; path?: string }): Promise<void>;
  };

  agents: {
    list(project?: string): Promise<Agent[]>;
    get(id: string): Promise<Agent>;
    create(def: Record<string, unknown>): Promise<Agent>;
    remove(id: string): Promise<void>;
    start(id: string): Promise<void>;
    stop(id: string): Promise<void>;
    restart(id: string): Promise<void>;
    send(id: string, text: string): Promise<void>;
    events(id: string, since?: string): Promise<AgentEvent[]>;
    sessions(id: string): Promise<Session[]>;
    roles(id: string): Promise<Role[]>;
    setRoles(id: string, roles: Role[]): Promise<Role[]>;
    /** Bidirectional PTY. See ./sockets for the protocol. */
    pty(id: string): PtySocket;
  };

  board: {
    get(project: string): Promise<Board>;
    update(project: string, columns: Column[]): Promise<Board>;
  };

  cards: {
    create(project: string, card: NewCard): Promise<Card>;
    get(id: string): Promise<Card>;
    update(id: string, patch: CardPatch): Promise<Card>;
    archive(id: string): Promise<void>;
    comment(id: string, body: string): Promise<Card>;
    events(id: string): Promise<AgentEvent[]>;
  };

  memories: {
    list(project: string, q?: MemoryQuery): Promise<Memory[]>;
    create(project: string, m: Pick<Memory, "title" | "body" | "tags">): Promise<Memory>;
    get(id: string): Promise<Memory>;
    update(id: string, patch: Partial<Pick<Memory, "title" | "body" | "tags">>): Promise<Memory>;
    remove(id: string): Promise<void>;
    graph(id: string, depth?: number): Promise<MemoryGraph>;
    link(id: string, to: string, relation: Relation, note?: string): Promise<MemoryLink>;
    unlink(id: string, to: string, relation: Relation): Promise<void>;
    supersede(id: string, by: string): Promise<void>;
  };

  wiki: {
    tree(project: string, q?: string): Promise<WikiPageSummary[]>;
    create(project: string, page: { path: string; title: string; body: string; summary: string }): Promise<WikiPage>;
    get(id: string): Promise<WikiPage>;
    save(id: string, page: { body: string; base: number; summary: string }): Promise<WikiPage>;
    remove(id: string): Promise<void>;
    section(id: string, s: { heading: string; body: string; base: number; summary: string }): Promise<WikiPage>;
    revisions(id: string): Promise<Revision[]>;
    revision(id: string, rev: number): Promise<Revision & { body: string }>;
    diff(id: string, a: number, b: number): Promise<{ diff: string }>;
    restore(id: string, rev: number): Promise<WikiPage>;
    backlinks(id: string): Promise<WikiPageSummary[]>;
    review(id: string, decision: "approve" | "comment", comment: string): Promise<WikiPage>;
  };

  artifacts: {
    list(project: string): Promise<Artifact[]>;
    upload(project: string, form: FormData): Promise<Artifact>;
    get(id: string): Promise<Artifact>;
    remove(id: string): Promise<void>;
    link(id: string, kind: Artifact["links"][number]["kind"], ref: string): Promise<void>;
    /** Text content for text, Markdown, and code previews. */
    text(a: Artifact): Promise<string>;
  };

  versions: {
    list(project: string): Promise<Version[]>;
    create(project: string, v: Pick<Version, "name" | "notes">): Promise<Version>;
    get(id: string): Promise<Version>;
    update(id: string, patch: Partial<Pick<Version, "notes" | "cards" | "artifacts">>): Promise<Version>;
    transition(id: string, action: "submit" | "approve" | "reject" | "release", comment?: string): Promise<Version>;
    compare(id: string, to: string): Promise<{ cards: string[]; pages: string[]; artifacts: string[] }>;
  };

  subscriptions: {
    list(): Promise<Subscription[]>;
    quota(name: string, since?: string): Promise<{ at: string; windows: QuotaWindow[] }[]>;
    probe(name: string): Promise<Subscription>;
    detect(): Promise<DetectedLogin[]>;
    startLogin(kind: LoginKind, provider?: string): Promise<LoginSession>;
    loginState(id: string): Promise<LoginSession>;
    loginInput(id: string, code: string): Promise<void>;
    loginPty(id: string): PtySocket;
    setApiKey(name: string, input: ApiKeyInput): Promise<Subscription>;
    logout(name: string): Promise<void>;
    verify(name: string): Promise<Subscription>;
  };

  usage(q: UsageQuery): Promise<UsageBucket[]>;

  mail: {
    list(q?: { to?: string; state?: MessageState }): Promise<Message[]>;
    send(m: NewMessage): Promise<Message>;
    markRead(id: string): Promise<void>;
    reply(id: string, body: string): Promise<Message>;
  };

  inbox: {
    list(project: string): Promise<Message[]>;
    send(project: string, m: Omit<NewMessage, "to">): Promise<Message>;
  };

  audit(q?: { since?: string; actor?: string }): Promise<AuditEntry[]>;

  targets: {
    list(): Promise<Target[]>;
    create(t: Partial<Target>): Promise<Target>;
    action(id: string, action: "up" | "down" | "snapshot" | "restore"): Promise<Target>;
  };

  /** Not yet in §17. Planned with the host agent (§19). */
  hosts: {
    list(): Promise<Host[]>;
  };

  /** The dashboard firehose (`/api/events`). */
  events(): EventStream;
}

export function createHttpApi(t: Transport, makeSocket: (url: string) => WebSocket = (u) => new WebSocket(u)): Api {
  const get = <T>(path: string, query?: Query) => t.request<T>("GET", path, { query });
  const post = <T>(path: string, body?: unknown) => t.request<T>("POST", path, { body });
  const patch = <T>(path: string, body: unknown) => t.request<T>("PATCH", path, { body });
  const put = <T>(path: string, body: unknown) => t.request<T>("PUT", path, { body });
  const del = (path: string) => t.request<void>("DELETE", path);
  const socket = (path: string): PtySocket => {
    const ws = makeSocket(t.wsUrl(path));
    ws.binaryType = "arraybuffer";
    return ws;
  };
  const p = (name: string) => `/api/projects/${seg(name)}`;

  return {
    health: () => get("/api/health"),

    projects: {
      list: () => get("/api/projects"),
      get: (name) => get(p(name)),
      create: (proj) => post("/api/projects", proj),
      update: (name, body) => patch(p(name), body),
      remove: (name) => del(p(name)),
      agents: (name) => get(`${p(name)}/agents`),
      targets: (name) => get(`${p(name)}/targets`),
      repos: (name) => get(`${p(name)}/repos`),
      attachRepo: (name, repo) => post(`${p(name)}/repos`, repo),
    },

    repos: {
      checkout: (id, opts) => post(`/api/repos/${seg(id)}/checkout`, opts),
    },

    agents: {
      list: (project) => get("/api/agents", { project }),
      get: (id) => get(`/api/agents/${seg(id)}`),
      create: (def) => post("/api/agents", def),
      remove: (id) => del(`/api/agents/${seg(id)}`),
      start: (id) => post(`/api/agents/${seg(id)}/start`),
      stop: (id) => post(`/api/agents/${seg(id)}/stop`),
      restart: (id) => post(`/api/agents/${seg(id)}/restart`),
      send: (id, text) => post(`/api/agents/${seg(id)}/send`, { text }),
      events: (id, since) => get(`/api/agents/${seg(id)}/events`, { since }),
      sessions: (id) => get(`/api/agents/${seg(id)}/sessions`),
      roles: (id) => get(`/api/agents/${seg(id)}/roles`),
      setRoles: (id, roles) => put(`/api/agents/${seg(id)}/roles`, roles),
      pty: (id) => socket(`/api/agents/${seg(id)}/pty`),
    },

    board: {
      get: (project) => get(`${p(project)}/board`),
      update: (project, columns) => patch(`${p(project)}/board`, { columns }),
    },

    cards: {
      create: (project, card) => post(`${p(project)}/cards`, card),
      get: (id) => get(`/api/cards/${seg(id)}`),
      update: (id, body) => patch(`/api/cards/${seg(id)}`, body),
      archive: (id) => del(`/api/cards/${seg(id)}`),
      comment: (id, body) => post(`/api/cards/${seg(id)}/comments`, { body }),
      events: (id) => get(`/api/cards/${seg(id)}/events`),
    },

    memories: {
      list: (project, q = {}) => get(`${p(project)}/memories`, { ...q }),
      create: (project, m) => post(`${p(project)}/memories`, m),
      get: (id) => get(`/api/memories/${seg(id)}`),
      update: (id, body) => patch(`/api/memories/${seg(id)}`, body),
      remove: (id) => del(`/api/memories/${seg(id)}`),
      graph: (id, depth = 1) => get(`/api/memories/${seg(id)}/graph`, { depth }),
      link: (id, to, relation, note) => post(`/api/memories/${seg(id)}/links`, { to, relation, note }),
      unlink: (id, to, relation) =>
        t.request("DELETE", `/api/memories/${seg(id)}/links`, { body: { to, relation } }),
      supersede: (id, by) => post(`/api/memories/${seg(id)}/supersede`, { by }),
    },

    wiki: {
      tree: (project, q) => get(`${p(project)}/wiki`, { q }),
      create: (project, page) => post(`${p(project)}/wiki`, page),
      get: (id) => get(`/api/wiki/${seg(id)}`),
      save: (id, page) => put(`/api/wiki/${seg(id)}`, page),
      remove: (id) => del(`/api/wiki/${seg(id)}`),
      section: (id, s) => post(`/api/wiki/${seg(id)}/section`, s),
      revisions: (id) => get(`/api/wiki/${seg(id)}/revisions`),
      revision: (id, rev) => get(`/api/wiki/${seg(id)}/revisions/${rev}`),
      diff: (id, a, b) => get(`/api/wiki/${seg(id)}/diff`, { a, b }),
      restore: (id, rev) => post(`/api/wiki/${seg(id)}/restore`, { rev }),
      backlinks: (id) => get(`/api/wiki/${seg(id)}/backlinks`),
      review: (id, decision, comment) => post(`/api/wiki/${seg(id)}/review`, { decision, comment }),
    },

    artifacts: {
      list: (project) => get(`${p(project)}/artifacts`),
      upload: (project, form) => t.request("POST", `${p(project)}/artifacts`, { form }),
      get: (id) => get(`/api/artifacts/${seg(id)}`),
      remove: (id) => del(`/api/artifacts/${seg(id)}`),
      link: (id, kind, ref) => post(`/api/artifacts/${seg(id)}/links`, { kind, ref }),
      text: async (a) => {
        const res = await fetch(a.url);
        return res.text();
      },
    },

    versions: {
      list: (project) => get(`${p(project)}/versions`),
      create: (project, v) => post(`${p(project)}/versions`, v),
      get: (id) => get(`/api/versions/${seg(id)}`),
      update: (id, body) => patch(`/api/versions/${seg(id)}`, body),
      transition: (id, action, comment) => post(`/api/versions/${seg(id)}/${action}`, { comment }),
      compare: (id, to) => get(`/api/versions/${seg(id)}/compare`, { to }),
    },

    subscriptions: {
      list: () => get("/api/subscriptions"),
      quota: (name, since) => get(`/api/subscriptions/${seg(name)}/quota`, { since }),
      probe: (name) => post(`/api/subscriptions/${seg(name)}/probe`),
      detect: () => get("/api/subscriptions/detect"),
      startLogin: (kind, provider) => post("/api/subscriptions/login", { kind, provider }),
      loginState: (id) => get(`/api/subscriptions/login/${seg(id)}`),
      loginInput: (id, code) => post(`/api/subscriptions/login/${seg(id)}/input`, { code }),
      loginPty: (id) => socket(`/api/subscriptions/login/${seg(id)}/pty`),
      setApiKey: (name, input) => post(`/api/subscriptions/${seg(name)}/api-key`, input),
      logout: (name) => post(`/api/subscriptions/${seg(name)}/logout`),
      verify: (name) => post(`/api/subscriptions/${seg(name)}/verify`),
    },

    usage: (q) => get("/api/usage", { ...q }),

    mail: {
      list: (q = {}) => get("/api/mail", { ...q }),
      send: (m) => post("/api/mail", m),
      markRead: (id) => post(`/api/mail/${seg(id)}/read`),
      reply: (id, body) => post(`/api/mail/${seg(id)}/reply`, { body }),
    },

    inbox: {
      list: (project) => get(`${p(project)}/inbox`),
      send: (project, m) => post(`${p(project)}/inbox`, m),
    },

    audit: (q = {}) => get("/api/audit", { ...q }),

    targets: {
      list: () => get("/api/targets"),
      create: (target) => post("/api/targets", target),
      action: (id, action) => post(`/api/targets/${seg(id)}/${action}`),
    },

    hosts: {
      list: () => get("/api/hosts"),
    },

    events: () => createWsEventStream(() => t.wsUrl("/api/events"), makeSocket),
  };
}

