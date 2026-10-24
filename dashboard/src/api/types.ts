// Domain types for the Terrarium API. They follow docs/PLAN.md §5 and §17.
// Timestamps are ISO 8601 strings. IDs are strings.

export type Harness = "claude" | "codex" | "copilot" | "pi";

export const AGENT_STATUSES = [
  "starting",
  "idle",
  "working",
  "blocked",
  "error",
  "stopped",
  "restarting",
] as const;
export type AgentStatus = (typeof AGENT_STATUSES)[number];

/** Roles above `member`. The daemon drops `member`; it is implicit. */
export type Role = "sysadmin" | "project_admin" | "member";
export const GRANTABLE_ROLES = ["project_admin", "sysadmin"] as const;

export type RestartPolicy = "always" | "on-failure" | "never";
export const RESTART_POLICIES: RestartPolicy[] = ["on-failure", "always", "never"];
export const HARNESSES: Harness[] = ["claude", "codex", "copilot", "pi"];

/** `file`: declared in a definition file, read-only over the API. `api`: made over the API. */
export type Source = "file" | "api";

/** `GET /api/health` (docs/api.md §4). */
export interface Health {
  status?: string;
  version: string;
  /** Seconds since the daemon started. */
  uptime: number;
  started_at?: string;
  definitions?: { projects: number; agents: number; targets: number; subscriptions: number };
}

/** docs/api.md §5. */
export interface Project {
  name: string;
  description?: string;
  card_prefix: string;
  source?: Source;
  created_at: string;
  updated_at?: string;
}

export interface NewProject {
  name: string;
  description?: string;
  card_prefix?: string;
}

/** docs/api.md §5, attach a repo. */
export interface Repo {
  id: string;
  project: string;
  name: string;
  url: string;
  default_branch?: string;
  /** A key name in `/state/credentials`, never the secret. */
  credential?: string;
  path?: string;
  source?: Source;
  created_at?: string;
}

export interface NewRepo {
  name?: string;
  url: string;
  default_branch?: string;
  credential?: string;
  path?: string;
}

export interface AgentPolicy {
  /** Target names. */
  targets: string[];
  /** Mail recipients in the project. `"*"` means every agent. */
  mail_to: string[];
  /** `project/agent` entries outside the project. The daemon leaves it out when empty. */
  cross_project?: string[];
}

/** The agent definition, the TOML file as JSON (docs/api.md §6.1). */
export interface AgentDef {
  name: string;
  project: string;
  harness: Harness;
  subscription?: string;
  model?: string;
  /** The prompt template name under `prompts/`. */
  prompt?: string;
  roles?: Role[];
  policy?: AgentPolicy;
  restart?: RestartPolicy;
}

/** `PATCH /api/agents/{id}`. Name, project, and roles do not change here. */
export type AgentPatch = Partial<Pick<AgentDef, "harness" | "subscription" | "model" | "prompt" | "policy" | "restart">>;

/** docs/api.md §6. */
export interface Agent {
  id: string;
  name: string;
  project: string;
  harness: Harness;
  subscription?: string;
  model?: string;
  prompt?: string;
  workspace: string;
  status: AgentStatus;
  /** Roles above `member`. */
  roles: Role[];
  policy?: AgentPolicy;
  restart?: RestartPolicy;
  /** The harness session ID of the current session, when known. */
  session?: string;
  /** ID of the card the agent has in `in_progress`, if any. */
  current_card?: string;
  source?: Source;
  created_at: string;
  updated_at?: string;
  /** Client-side: when the open turn started, from the event stream. */
  turn_started_at?: string;
  /** Client-side: the last event seen for the agent. */
  last_event?: AgentEvent;
}

/** docs/api.md §6.3. */
export interface Session {
  id: string;
  agent: string;
  harness_session_id?: string;
  started_at: string;
  ended_at?: string;
  /** `stopped`, `restarted`, `error`, `start_failed`, `superseded`, or a reason. */
  outcome?: string;
  turns: number;
  current: boolean;
}

/** Event kinds of docs/api.md §9, then kinds that §14 to §16 will add. */
export const EVENT_KINDS = [
  "status",
  "session_started",
  "turn_start",
  "turn_end",
  "tool_use",
  "tool_finished",
  "notification",
  "question",
  "session_ended",
  "exit",
  "delivery_failed",
  "system",
  "mail",
  "card",
  "board",
  "memory.created",
  "memory.updated",
  "memory.deleted",
  "memory.linked",
  "memory.unlinked",
  "memory.superseded",
  "wiki.created",
  "wiki.saved",
  "wiki.deleted",
  "wiki.restored",
  "wiki.moved",
  "wiki.reviewed",
  "artifact.created",
  "artifact.linked",
  "artifact.deleted",
  "artifact.pruned",
  "version.created",
  "version.updated",
  "version.submitted",
  "version.approved",
  "version.rejected",
  "version.released",
  // Planned (PLAN §16). A guessed name; the daemon does not emit it yet.
  "subscription",
] as const;
export type EventKind = (typeof EVENT_KINDS)[number];

/** One row of the event log (docs/api.md §9). */
export interface AgentEvent {
  /** The position in the log. Use it as `?since=`. */
  seq: number;
  id: string;
  at: string;
  kind: EventKind;
  /** `operator`, `system`, or `agent:<ulid>`. */
  actor?: string;
  /** The agent **name**. `agent_id` and `data.agent_id` carry the ULID. */
  agent?: string;
  agent_id?: string;
  project?: string;
  card?: string;
  session?: string;
  /** Short human summary, for lists. */
  summary: string;
  /** Kind-specific payload. `status`: `{status, from, reason}`. `card`: `{card_id, change, ...}`. */
  data: Record<string, unknown>;
}

/** Query of the event history: `GET /api/events` and `GET /api/agents/{id}/events`. */
export interface EventQuery {
  /** A `seq` or an event ID. Only later events come. */
  since?: number | string;
  kind?: string;
  project?: string;
  agent?: string;
  limit?: number;
  cursor?: string;
}

// ----- Mail (§12, docs/api.md §7) -----

export type MessageState = "queued" | "delivered" | "read" | "replied";
export type AddressKind = "operator" | "project" | "agent";

export interface Message {
  id: string;
  thread_id: string;
  /** Stored address: `operator`, `project:<name>`, or the agent ULID. */
  from: string;
  from_kind: AddressKind;
  /** Readable: `operator`, `project:<name>`, or `project/agent`. */
  from_name: string;
  to: string;
  to_kind: AddressKind;
  to_name: string;
  subject: string;
  body: string;
  state: MessageState;
  in_reply_to?: string;
  created_at: string;
  delivered_at?: string;
  read_at?: string;
}

export interface NewMessage {
  from?: string;
  to: string;
  subject?: string;
  body: string;
  thread_id?: string;
}

export interface MailQuery {
  to?: string;
  from?: string;
  thread?: string;
  /** A state, or `unread` (`queued` and `delivered`). */
  state?: MessageState | "unread";
  limit?: number;
  cursor?: string;
}

export interface InboxQuery {
  unread?: boolean;
  state?: MessageState;
  thread?: string;
  limit?: number;
  cursor?: string;
}

// ----- Board (§13, docs/api.md §8) -----

export interface Column {
  name: string;
  position: number;
  wip_limit?: number;
  /** Cards in the column, archived ones excluded. */
  count?: number;
}

/** A column in `PATCH /api/projects/{name}/board`. */
export interface ColumnEdit {
  name: string;
  wip_limit?: number;
  rename_from?: string;
}

export interface CardComment {
  id?: string;
  author: string;
  author_name?: string;
  body: string;
  created_at: string;
}

export type CardLinkKind = "thread" | "branch" | "card" | "url";

export interface CardLink {
  kind: CardLinkKind;
  ref: string;
  created_by?: string;
  created_at?: string;
}

export interface Card {
  id: string;
  project: string;
  column: string;
  position: number;
  title: string;
  body: string;
  /** Stored address: an agent ULID or `operator`. */
  assignee?: string;
  /** Readable: `project/agent` or `operator`. */
  assignee_name?: string;
  labels: string[];
  repo?: string;
  branch?: string;
  depends_on: string[];
  /** Dependencies that are neither `done` nor archived. */
  blocked_by: string[];
  comments: CardComment[];
  links: CardLink[];
  created_by: string;
  created_by_name?: string;
  created_at: string;
  updated_at: string;
  archived: boolean;
}

export interface Board {
  project: string;
  card_prefix?: string;
  columns: Column[];
  cards: Card[];
}

export interface NewCard {
  /** Required unless `from_mail` supplies it. */
  title?: string;
  body?: string;
  column?: string;
  assignee?: string;
  labels?: string[];
  repo?: string;
  branch?: string;
  depends_on?: string[];
  from_mail?: string;
  comment?: string;
}

export interface CardPatch {
  title?: string;
  body?: string;
  column?: string;
  position?: number;
  assignee?: string | null;
  labels?: string[];
  repo?: string | null;
  branch?: string | null;
  depends_on?: string[];
  /** Required by the daemon when `column` is `blocked`. */
  comment?: string;
}

export interface CardQuery {
  column?: string;
  assignee?: string;
  unassigned?: boolean;
  label?: string;
  archived?: boolean;
  limit?: number;
  cursor?: string;
}

// ----- Memory (§14, docs/api.md §10) -----

export const RELATIONS = [
  "relates_to",
  "supports",
  "contradicts",
  "supersedes",
  "derived_from",
  "part_of",
  "example_of",
] as const;
export type Relation = (typeof RELATIONS)[number];

export type MemorySourceKind = "card" | "thread" | "path" | "commit" | "url";

/** One edge as a memory shows it: both directions, with the other end's title. */
export interface MemoryEdge {
  from: string;
  to: string;
  relation: Relation;
  /** `out` from this memory, `in` to it. */
  direction?: "out" | "in";
  other?: string;
  other_title?: string;
  other_superseded?: boolean;
  note?: string;
  author: string;
  author_name?: string;
  created_at: string;
}

export interface Memory {
  /** Project-local: `MEM-<n>`. */
  id: string;
  /** `project/MEM-<n>`. Use it in paths; a bare ID is ambiguous for the operator. */
  key: string;
  project: string;
  title: string;
  body: string;
  tags: string[];
  author: string;
  author_name?: string;
  sources: { kind: MemorySourceKind; ref: string }[];
  supersedes: string[];
  superseded_by?: string;
  links: MemoryEdge[];
  created_at: string;
  updated_at: string;
}

export interface NewMemory {
  title: string;
  body?: string;
  tags?: string[];
  sources?: Memory["sources"];
  links?: { to: string; relation: Relation; note?: string }[];
}

export interface MemoryPatch {
  title?: string;
  body?: string;
  tags?: string[];
  add_tags?: string[];
  remove_tags?: string[];
  sources?: Memory["sources"];
}

/** The create answer adds `similar`: the top hits for the title. */
export type CreatedMemory = Memory & { similar: { id: string; title: string; superseded: boolean }[] };

export interface MemoryGraph {
  root: string;
  depth: number;
  nodes: Memory[];
  edges: MemoryEdge[];
}

export interface MemoryQuery {
  q?: string;
  tag?: string;
  author?: string;
  all?: boolean;
}

// ----- Wiki (§15.1, docs/api.md §11) -----

export type PageStatus = "draft" | "ready_for_review" | "reviewed";

export interface WikiLink {
  kind: "page" | "card" | "memory" | "artifact" | "version" | string;
  /** `project/path`, a card ID, `project/MEM-n`, `project/ART-n`, or `project/<version>`. */
  target: string;
}

/** A page in a list: no body. Search hits add `snippet`. */
export interface WikiPageSummary {
  id: string;
  project: string;
  path: string;
  title: string;
  rev: number;
  /** From the front matter. Absent when the page has none. */
  status?: PageStatus;
  reviewer?: string;
  updated_at: string;
  author?: string;
  author_name?: string;
  snippet?: string;
  deleted_at?: string;
}

export interface WikiPage extends WikiPageSummary {
  body: string;
  author: string;
  author_name: string;
  /** The summary of the current revision. */
  summary: string;
  created_by: string;
  created_at: string;
  links: WikiLink[];
}

export interface Revision {
  page: string;
  rev: number;
  author: string;
  author_name?: string;
  summary: string;
  created_at: string;
}

export interface WikiDiff {
  page: string;
  a: number;
  b: number;
  diff: string;
}

// ----- Artifacts (§15.2, docs/api.md §12) -----

export type ArtifactLinkKind = "page" | "version" | "card";

export interface Artifact {
  /** Project-local: `ART-<n>`. */
  id: string;
  /** `project/ART-<n>`. */
  key: string;
  /** The ULID in serving URLs. */
  uid: string;
  project: string;
  name: string;
  content_type: string;
  size: number;
  sha256: string;
  kind: "file" | "bundle";
  /** The viewer may show it inline. Else it offers a download. */
  renders: boolean;
  created_by: string;
  created_by_name?: string;
  created_at: string;
  links: { kind: ArtifactLinkKind; ref: string; label?: string; created_by?: string; created_at?: string }[];
  /** Live pages that embed it, as `project/path`. */
  embedded_in: string[];
}

/** `POST /api/artifacts/{id}/url`: a short-lived signed URL on the serving routes. */
export interface ArtifactUrl {
  url: string;
  /** Unix time. */
  exp: number;
  expires_at: string;
}

export interface ArtifactAttach {
  kind: ArtifactLinkKind;
  /** A page key, a version key, or a card ID of the same project. */
  ref: string;
}

/** `POST /api/projects/{name}/artifacts` as JSON (docs/api.md §12.2). */
export interface ArtifactJsonUpload {
  name: string;
  content_type?: string;
  data_base64: string;
  kind?: "file" | "bundle";
  attach?: ArtifactAttach;
}

export interface ArtifactUsage {
  project: string;
  count: number;
  used_bytes: number;
  quota_bytes: number;
  max_file_bytes: number;
}

// ----- Versions (§15.3, docs/api.md §13) -----

export type VersionStatus = "draft" | "in_review" | "approved" | "rejected" | "released";

export interface VersionReview {
  id?: string;
  author: string;
  author_name?: string;
  decision: "submitted" | "approved" | "rejected" | "released";
  comment: string;
  created_at: string;
}

export interface Version {
  id: string;
  project: string;
  name: string;
  status: VersionStatus;
  notes: string;
  repo?: string;
  git_ref?: string;
  thread_id?: string;
  created_by: string;
  created_by_name?: string;
  created_at: string;
  updated_at?: string;
  submitted_at?: string;
  submitted_by?: string;
  decided_at?: string;
  decided_by?: string;
  released_at?: string;
  /** The dashboard page of the version. */
  url?: string;
  cards: { id: string; title: string; column: string }[];
  artifacts: { id: string; uid: string; name: string; kind: Artifact["kind"]; content_type: string; size: number }[];
  reviews: VersionReview[];
}

export interface NewVersion {
  name: string;
  notes?: string;
  repo?: string;
  git_ref?: string;
  cards?: string[];
  artifacts?: string[];
}

export interface VersionPatch {
  name?: string;
  notes?: string;
  repo?: string;
  git_ref?: string;
  cards?: string[];
  artifacts?: string[];
  add_cards?: string[];
  remove_cards?: string[];
  add_artifacts?: string[];
  remove_artifacts?: string[];
}

export interface VersionCompare {
  version: string;
  to: string;
  since?: string;
  until?: string;
  cards: { added: string[]; removed: string[]; landed: string[] };
  artifacts: { added: string[]; removed: string[] };
  pages: { id: string; path: string; from_rev?: number; to_rev: number }[];
}

// ----- Subscriptions and usage (§16) -----

export type SubscriptionStatus = "logged_in" | "expiring" | "expired" | "missing";

export interface QuotaWindow {
  /** Window label, for example `5h`, `7d`, `monthly`. */
  window: string;
  used_pct: number;
  resets_at: string;
}

export interface Subscription {
  name: string;
  provider: string;
  plan: string;
  harnesses: Harness[];
  /** Probe name, or null when only local accounting exists. */
  probe: string | null;
  alert_at: number;
  status: SubscriptionStatus;
  verified_at?: string;
  windows: QuotaWindow[];
  /** Used percent of the busiest window, sampled hourly over 24 hours. */
  history: number[];
  last_poll_at?: string;
}

export interface DetectedLogin {
  harness: Harness;
  provider: string;
  plan?: string;
  account?: string;
}

export type LoginKind = "claude" | "codex" | "copilot" | "pi_oauth" | "pi_api_key";

export interface LoginSession {
  id: string;
  kind: LoginKind;
  provider?: string;
  state: "starting" | "waiting_for_user" | "verifying" | "done" | "error";
  url?: string;
  device_code?: string;
  error?: string;
  /** Set when state is `done`. */
  subscription?: string;
}

export interface ApiKeyInput {
  provider: string;
  key: string;
  base_url?: string;
  models?: string[];
}

export interface UsageQuery {
  subscription?: string;
  project?: string;
  agent?: string;
  model?: string;
  since?: string;
  bucket?: "hour" | "day";
}

export interface UsageBucket {
  at: string;
  subscription: string;
  project: string;
  agent: string;
  model: string;
  input_tokens: number;
  output_tokens: number;
  cache_tokens: number;
  turns: number;
}

// ----- Targets and hosts (§19) -----

export type TargetKind = "host" | "ssh" | "docker" | "vmlab";

/** docs/api.md §4 `GET /api/targets`. */
export interface Target {
  /** The target name. Targets have no other ID yet. */
  id: string;
  name: string;
  kind: TargetKind;
  project?: string;
  host?: string;
  address?: string;
  /** `global` or `project`. */
  scope?: "global" | "project";
  state: "up" | "down" | "starting" | "stopping" | "unknown";
  ephemeral: boolean;
  keep?: boolean;
  snapshot_before_task?: boolean;
  snapshots: string[];
}

export interface Host {
  name: string;
  connected: boolean;
  os: string;
  arch: string;
  capabilities: { docker: boolean; vmlab: boolean };
  last_seen: string;
  version: string;
}

// ----- Audit -----

/** docs/api.md §4 `GET /api/audit`. */
export interface AuditEntry {
  id: string;
  seq?: number;
  at: string;
  /** `operator`, `system`, or `agent:<ulid>`. */
  actor: string;
  action: string;
  target: string;
  project?: string;
  /** A JSON object. */
  detail?: Record<string, unknown> | null;
}

export interface AuditQuery {
  since?: string;
  actor?: string;
  action?: string;
  project?: string;
  limit?: number;
  cursor?: string;
}
