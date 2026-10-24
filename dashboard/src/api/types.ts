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

export type Role = "sysadmin" | "project_admin" | "member";

export interface Health {
  version: string;
  /** Seconds since the daemon started. */
  uptime: number;
}

export interface Project {
  name: string;
  description: string;
  card_prefix: string;
  created_at: string;
}

export interface Repo {
  id: string;
  project: string;
  url: string;
  default_branch: string;
  path: string;
}

export interface Agent {
  id: string;
  name: string;
  project: string;
  harness: Harness;
  subscription: string;
  model?: string;
  status: AgentStatus;
  roles: Role[];
  /** ID of the card the agent has in `in_progress`, if any. */
  current_card?: string;
  /** When the open turn started, if the agent is working. */
  turn_started_at?: string;
  last_event?: AgentEvent;
  workspace: string;
  created_at: string;
}

export interface Session {
  id: string;
  agent: string;
  harness_session_id: string;
  started_at: string;
  ended_at?: string;
  turns: number;
  current: boolean;
}

export type EventKind =
  | "turn_start"
  | "turn_end"
  | "tool_call"
  | "status"
  | "exit"
  | "mail"
  | "card"
  | "memory"
  | "wiki"
  | "version"
  | "subscription";

export interface AgentEvent {
  id: string;
  at: string;
  kind: EventKind;
  project?: string;
  agent?: string;
  /** Short human summary, for lists. */
  summary: string;
  /** Kind-specific payload. `status` events carry `{ status }`. */
  data?: Record<string, unknown>;
}

// ----- Mail (§12) -----

export type MessageState = "queued" | "delivered" | "read" | "replied";

export interface Message {
  id: string;
  thread_id: string;
  from: string;
  to: string;
  subject: string;
  body: string;
  state: MessageState;
  created_at: string;
}

export interface NewMessage {
  from?: string;
  to: string;
  subject: string;
  body: string;
  thread_id?: string;
}

// ----- Board (§13) -----

export interface Column {
  name: string;
  position: number;
  wip_limit?: number;
}

export interface CardComment {
  author: string;
  body: string;
  created_at: string;
}

export interface Card {
  id: string;
  project: string;
  column: string;
  position: number;
  title: string;
  body: string;
  assignee?: string;
  labels: string[];
  repo?: string;
  branch?: string;
  depends_on: string[];
  comments: CardComment[];
  links: { kind: string; ref: string }[];
  created_by: string;
  created_at: string;
  updated_at: string;
}

export interface Board {
  project: string;
  columns: Column[];
  cards: Card[];
}

export interface NewCard {
  title: string;
  body?: string;
  column?: string;
  assignee?: string;
  labels?: string[];
  from_mail?: string;
}

export interface CardPatch {
  title?: string;
  body?: string;
  column?: string;
  position?: number;
  assignee?: string | null;
  labels?: string[];
  /** Required by the daemon when `column` is `blocked`. */
  comment?: string;
}

// ----- Memory (§14) -----

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

export interface Memory {
  id: string;
  project: string;
  title: string;
  body: string;
  tags: string[];
  author: string;
  sources: { kind: "card" | "thread" | "path" | "commit"; ref: string }[];
  superseded_by?: string;
  created_at: string;
  updated_at: string;
}

export interface MemoryLink {
  from: string;
  to: string;
  relation: Relation;
  author: string;
  note?: string;
  created_at: string;
}

export interface MemoryGraph {
  nodes: Memory[];
  links: MemoryLink[];
}

export interface MemoryQuery {
  q?: string;
  tag?: string;
  author?: string;
  all?: boolean;
}

// ----- Wiki (§15.1) -----

export type PageStatus = "draft" | "ready_for_review" | "reviewed";

export interface WikiPageSummary {
  id: string;
  project: string;
  path: string;
  title: string;
  status: PageStatus;
  current_rev: number;
  updated_at: string;
  updated_by: string;
}

export interface WikiPage extends WikiPageSummary {
  body: string;
  reviewer?: string;
}

export interface Revision {
  rev: number;
  author: string;
  summary: string;
  created_at: string;
}

// ----- Artifacts (§15.2) -----

export interface Artifact {
  id: string;
  project: string;
  name: string;
  content_type: string;
  size: number;
  sha256: string;
  kind: "file" | "bundle";
  created_by: string;
  created_at: string;
  /** Short-lived signed URL on the artifact origin. */
  url: string;
  links: { kind: "page" | "version" | "card"; ref: string }[];
}

// ----- Versions (§15.3) -----

export type VersionStatus = "draft" | "in_review" | "approved" | "rejected" | "released";

export interface VersionReview {
  author: string;
  decision: "approve" | "reject" | "comment";
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
  cards: string[];
  artifacts: string[];
  reviews: VersionReview[];
  created_by: string;
  created_at: string;
  submitted_at?: string;
  decided_at?: string;
  decided_by?: string;
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

export interface Target {
  id: string;
  name: string;
  kind: TargetKind;
  project?: string;
  host?: string;
  state: "up" | "down" | "starting" | "stopping" | "unknown";
  ephemeral: boolean;
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

export interface AuditEntry {
  id: string;
  at: string;
  actor: string;
  action: string;
  target: string;
  detail?: string;
}
