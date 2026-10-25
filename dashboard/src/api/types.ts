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
  /** Client-side: the agent's last `self_report` (`terrarium status set`). Not the runner status. */
  self_report?: { status: string; note?: string; at: string };
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

/** Event kinds of docs/api.md §9 and §16.7. */
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
  // docs/api.md §16.7.
  "usage.sample",
  "quota.sample",
  "quota.alert",
  "quota.blocked",
  "quota.resumed",
  "subscription.offer",
  "self_report",
  // docs/api.md §9, §17.7.
  "lease",
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
  read_by?: string;
  read_by_name?: string;
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
  /** The dashboard page of the version: a path, `/p/<project>/versions/<id>`, or an absolute URL when the daemon has `TERRARIUM_PUBLIC_URL`. */
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

// ----- Subscriptions and usage (docs/api.md §16) -----

/** `unknown`: not checked yet. */
export type LoginStatus = "logged_in" | "expiring" | "expired" | "missing" | "unknown";
/** `none`: no probe, or not run yet. */
export type ProbeStatus = "ok" | "failed" | "none";

/** The newest sample of one window (docs/api.md §16.1). */
export interface QuotaWindow {
  /** `5h`, `7d`, `7d_opus`, `<n>h`, `<n>d`, `monthly`, ... */
  window: string;
  used_pct: number;
  resets_at?: string;
  /** When the probe read it. */
  at: string;
}

/** docs/api.md §16.1. */
export interface Subscription {
  name: string;
  provider: string;
  plan?: string;
  harnesses: Harness[];
  probe?: string;
  alert_at: number;
  source: Source;
  /** Set when an API key is stored: the env var that carries it. The key is never in the API. */
  env_var?: string;
  login_status: LoginStatus;
  verified_at?: string;
  probe_status: ProbeStatus;
  probe_error?: string;
  last_probe_at?: string;
  /** Stays when a probe fails. */
  last_success_at?: string;
  created_at: string;
  updated_at: string;
  windows: QuotaWindow[];
  /** No probe: local accounting is the only number. */
  local_estimate_only: boolean;
  has_api_key: boolean;
  /** Agents whose turns wait for a reset of this subscription. */
  held_agents: number;
}

/** `POST /api/subscriptions`, and the `suggested` definition of a detected login. */
export interface SubscriptionDef {
  name?: string;
  provider: string;
  plan?: string;
  harnesses?: Harness[];
  probe?: string;
  alert_at?: number;
}

export type NewSubscription = SubscriptionDef & { name: string };

/** `GET /api/subscriptions/{name}/quota`: one sample, oldest first. */
export interface QuotaSample {
  subscription: string;
  window: string;
  used_pct: number;
  resets_at?: string;
  /** The provider's window object. */
  raw?: unknown;
  at: string;
}

/** `POST /api/subscriptions/{name}/probe`. A failed probe is still `200`. */
export interface ProbeOutcome {
  subscription: string;
  probe_status: ProbeStatus;
  error?: string;
  windows: { window: string; used_pct: number; resets_at?: string; raw?: unknown }[];
}

/** A login check of one harness (and one pi provider). */
export interface LoginCheck {
  kind: LoginKind;
  harness: Harness;
  provider: string;
  pi_provider?: string;
  logged_in: boolean;
  plan?: string;
  detail?: string;
}

/** `POST /api/subscriptions/{name}/verify`. */
export interface VerifyResult {
  subscription: Subscription;
  status: Partial<LoginCheck> & { logged_in: boolean };
}

/** `GET /api/subscriptions/detect`: a login that no subscription covers. */
export interface DetectedLogin extends LoginCheck {
  /** The subscription that covers it. The daemon leaves these out of the list. */
  subscription?: string;
  suggested: SubscriptionDef;
}

/** The kinds of the login wizard (docs/api.md §16.4). */
export type LoginKind = "claude" | "codex" | "copilot" | "pi";
export const LOGIN_KINDS: LoginKind[] = ["claude", "codex", "copilot", "pi"];

export type LoginState = "starting" | "waiting" | "verifying" | "done" | "failed" | "cancelled" | "expired";

/** A login session (docs/api.md §16.4). */
export interface LoginSession {
  id: string;
  kind: LoginKind;
  /** For `pi`: the provider. */
  provider?: string;
  state: LoginState;
  url?: string;
  device_code?: string;
  /** Set when `state` is `done`: the subscription made or updated. */
  subscription?: string;
  started_at: string;
  finished_at?: string;
  error?: string;
  /** True for `done`, `failed`, `cancelled`, and `expired`. */
  done: boolean;
  instructions: string;
}

/** `POST /api/subscriptions/login`. */
export interface StartLogin {
  kind: LoginKind;
  provider?: string;
  name?: string;
}

/** `POST /api/subscriptions/{name}/api-key`. Write-only: no endpoint returns the key. */
export interface ApiKeyInput {
  provider: string;
  key: string;
  base_url?: string;
  models?: string[];
}

export type UsageGroup = "subscription" | "project" | "agent" | "model" | "card";

export interface UsageQuery {
  subscription?: string;
  project?: string;
  /** An agent key (docs/api.md §2.2). */
  agent?: string;
  model?: string;
  /** A card ID (`TER-42`): only the samples attributed to that card. */
  card?: string;
  /** RFC 3339. Default: 24 hours ago. */
  since?: string;
  until?: string;
  bucket?: "hour" | "day";
  group?: UsageGroup;
}

export interface UsageTotals {
  input_tokens: number;
  output_tokens: number;
  cache_read: number;
  cache_write: number;
  /** API responses. */
  messages: number;
  /** Distinct turns. */
  turns: number;
}

/** One point of `GET /api/usage`, oldest first. */
export interface UsagePoint extends UsageTotals {
  /** The start of the bucket. */
  bucket: string;
  /** The value of the `group` column when the series is split. For `agent` it is the agent ULID; for `card`, the card ID (absent: no card). */
  key?: string;
  /** With `group=card`: the card's title. */
  title?: string;
  /** Without `group=model`: the model, when all samples of the point have one model. */
  model?: string;
}

/** `GET /api/cards/{id}/usage` (docs/api.md §16.3): the samples attributed to one card. */
export interface CardUsage {
  card: string;
  title: string;
  project: string;
  since?: string;
  until?: string;
  total: UsageTotals;
  /** Biggest spender first. */
  models: (UsageTotals & { model: string })[];
  /** Biggest spender first. `name` is `project/name`. */
  agents: (UsageTotals & { id: string; name: string })[];
}

/** A hold on an agent's new turns (docs/api.md §16.6). */
export interface QuotaHold {
  agent: string;
  /** `quota:<subscription>` or `budget:project:<p>[/<sub>]`. */
  cause: string;
  until?: string;
  reason: string;
  /** False: the supervisor cannot hold turns, so the hold is advice only. */
  enforced: boolean;
  created_at?: string;
}

/** `GET /api/usage/me?agent=`. */
export interface UsageMe {
  agent: { id: string; name: string; project: string };
  subscription?: Pick<Subscription, "name" | "provider" | "plan" | "probe_status" | "last_success_at" | "local_estimate_only" | "windows">;
  today: { since: string; mine: UsageTotals; subscription?: UsageTotals; share_pct?: number };
  holds: QuotaHold[];
}

// ----- Targets and hosts (PLAN §17, §19, docs/host-protocol.md) -----
// Coded to the daemon's `api/hosts.rs` and `api/targets.rs`, which are new
// and not yet in docs/api.md. The mock serves the same shapes.

export type TargetKind = "host" | "ssh" | "docker" | "vmlab";
/** docs/api.md §17.2: what the last lifecycle verb saw. */
export type TargetState = "running" | "stopped" | "absent" | "unknown";
/** docs/api.md §17.2: `global` (no `project`) or `project`. */
export type TargetScope = "global" | "project";

/** docs/api.md §4 `GET /api/targets`. */
export interface Target {
  /** The target name. Targets have no other ID yet. */
  id: string;
  name: string;
  kind: TargetKind;
  project?: string;
  host?: string;
  address?: string;
  /** The fields of a target file (docs/api.md §17.2), by kind. */
  port?: number;
  credential?: string;
  image?: string;
  container?: string;
  lab?: string;
  machine?: string;
  /**
   * `global` or `project`. The client sets it on every target it reads: a
   * daemon without the field gets it from `project` (no project: global).
   */
  scope?: TargetScope;
  /** The state that the last lifecycle verb saw. */
  state: TargetState;
  state_at?: string | null;
  ephemeral: boolean;
  keep?: boolean;
  snapshot_before_task?: boolean;
  snapshots: string[];
  source?: Source;
  /** Whether the host agent is connected; `null` for `ssh`. */
  host_online?: boolean | null;
  /** Whether the caller may exec on it. */
  usable?: boolean;
  /** Why the caller may use it (docs/api.md §17.2): `operator`, `policy`, `lease`, or `null`. */
  access?: "operator" | "policy" | "lease" | null;
  /** The caller's active lease on the target, when `access` is `lease`; else `null`. */
  lease?: Lease | null;
}

/**
 * `POST /api/targets` (docs/api.md §17.2): a target as in a target file,
 * with `name`. No `project`: a global target (sysadmin only).
 */
export interface NewTarget {
  name: string;
  kind: TargetKind;
  project?: string;
  host?: string;
  address?: string;
  port?: number;
  credential?: string;
  image?: string;
  container?: string;
  lab?: string;
  machine?: string;
  ephemeral?: boolean;
  keep?: boolean;
  snapshot_before_task?: boolean;
}

// ----- Target leases (docs/api.md §17.7) -----

export type LeaseState = "active" | "revoked" | "expired";

/** One lease: one agent holds one target, for a time or until a revoke. */
export interface Lease {
  id: string;
  target: string;
  /** The holder's ULID. */
  agent: string;
  /** `project/name` of the holder; `null` after the agent is deleted. */
  agent_name: string | null;
  /** The holder's project. */
  project: string;
  /** `operator`, `agent:<ulid>`, or `system`. */
  granted_by: string;
  granted_at: string;
  /** The end. `null`: until a revoke. */
  expires_at: string | null;
  /** No other agent may hold a lease on the target at the same time. */
  exclusive: boolean;
  revoked_at: string | null;
  /** `operator`, `agent:<ulid>`, or `system` (the expiry). */
  revoked_by: string | null;
  reason: string | null;
  state: LeaseState;
}

/** `POST /api/targets/{name}/leases`. `until`: an RFC 3339 time or a duration (`30m`); absent: until a revoke. */
export interface LeaseGrant {
  /** An agent key (§2.2). */
  agent: string;
  until?: string | null;
  exclusive?: boolean;
  reason?: string;
}

/** The `201` of a grant: the lease, and the lifecycle verb that the grant ran, if any. */
export type LeaseGranted = Lease & { lifecycle?: unknown; lifecycle_error?: string };

/** `details` of `409 lease_held` (docs/api.md §2.5). */
export interface LeaseHeldDetails {
  target: string;
  /** `project/name`, else the ULID. */
  holder: string;
  holder_id: string;
  lease_id: string;
  exclusive: boolean;
  expires_at: string | null;
}

/** `data` of a `lease` event (docs/api.md §9). The event's `agent_id` is the holder. */
export interface LeaseEventData {
  change: "granted" | "extended" | "ended";
  target: string;
  lease_id: string;
  expires_at: string | null;
  exclusive: boolean;
  state: LeaseState;
}

/** The answer of a lifecycle verb (docs/api.md §17.3): the executor report, not the target. */
export interface TargetReport {
  target: string;
  verb: string;
  state: TargetState;
  snapshot?: string;
  result?: unknown;
}

/** `POST /api/targets/{name}/exec`. `shell` opens a login shell in a PTY. */
export interface ExecRequest {
  cmd?: string[];
  cwd?: string;
  env?: Record<string, string>;
  /** `true` (24x80) or a size. */
  pty?: boolean | { rows: number; cols: number };
  shell?: boolean;
  timeout_secs?: number;
}

export interface ExecOpened {
  session_id: string;
  target: string;
  kind: string;
}

/** `terrarium_core::protocol::ToolCapability`. */
export interface ToolCapability {
  present: boolean;
  version?: string;
}

/** `terrarium_core::protocol::HostCapabilities` (docs/host-protocol.md, Handshake). */
export interface HostCapabilities {
  docker?: ToolCapability;
  vmlab?: ToolCapability;
  shell?: string;
}

/** `GET /api/hosts`: a host agent in the daemon's registry, with its link state. */
export interface Host {
  name: string;
  id?: string;
  host_id?: string;
  /** `online` while the host agent's link is up. */
  status: "online" | "offline";
  online?: boolean;
  /** Live exec sessions. */
  sessions?: number;
  /** From the last `hello`. `{}` before the first connect. */
  capabilities?: HostCapabilities;
  /** `HostCapabilities::summary`: the flat view for policy checks. */
  summary?: { os: string; arch: string; docker: boolean; vmlab: boolean };
  last_seen?: string | null;
  os?: string | null;
  arch?: string | null;
  version?: string | null;
  max_sessions?: number;
  connected_at?: string;
  created_at?: string;
  created_by?: string;
}

/** `POST /api/hosts` (`201`): the host and its token. The token shows once, here only. */
export type NewHostToken = Host & { token: string };

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

/**
 * The answer of `POST /api/session` (docs/api.md §1.1). The cookie itself
 * comes in `Set-Cookie` (`HttpOnly`), so script never sees it.
 */
export interface SessionGrant {
  /** Send it as `X-CSRF-Token` on every POST, PUT, PATCH, and DELETE. */
  csrf: string;
  /** When the cookie ends, RFC 3339. */
  expires_at: string;
  /** The same time as a Unix time, in seconds. */
  exp?: number;
}
