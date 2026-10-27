// Domain types for the Terrarium API. They follow docs/PLAN.md §5 and §17.
// Timestamps are ISO 8601 strings. IDs are strings.

export type Harness = "claude" | "codex" | "copilot" | "pi";

export const AGENT_STATUSES = [
  "starting",
  "idle",
  /** Idle, with one or more open waits (docs/api.md, waits). A live status: it takes turns and mail as `idle` does. */
  "waiting",
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

/** `GET /api/harnesses` (docs/api.md §4): one harness against its pin and tested range. */
export interface HarnessVersion {
  harness: Harness;
  package: string;
  /** The version of the first install (harnesses.toml). */
  pinned: string;
  /** The range the adapter accepts. */
  tested: string;
  /** What `<binary> --version` reported; null when it did not answer. */
  installed: string | null;
  /** Null when the installed version is not known. */
  in_range: boolean | null;
  error?: string;
  checked_at: string | null;
  /** `?latest=true` only: the latest release on npm; null when npm did not answer (`latest_error` says why). */
  latest?: HarnessLatest | null;
  /** `?latest=true` only: the bump from `installed` (else `pinned`) to `latest` (`scripts/harness.py`); null when `latest` is null. */
  update?: HarnessUpdateKind | null;
  /** `?latest=true` only, when `latest` is null: why. */
  latest_error?: string;
}

/** `dist-tags.latest` of the package on npm, and its time. */
export interface HarnessLatest {
  version: string;
  published_at: string | null;
}

/** `none`: the pin is the latest. `major`: the first non-zero part changes (for 0.x, the second number). */
export type HarnessUpdateKind = "none" | "minor" | "major";

/** `POST /api/harnesses/{name}/update` (`200`), and one item of `POST /api/harnesses/update`. */
export interface HarnessUpdateResult {
  /** A harness, or the extension `copilot-prompt`. */
  harness: string;
  package: string;
  /** The version before the update; null when it was not installed. */
  from: string | null;
  /** The version after the update; null when it is not known. */
  to: string | null;
  /** What npm printed. */
  output: string;
  /** The adapter's tested range; null for the extension. */
  tested: string | null;
  /** Is `to` in `tested`? Null when it is not known. */
  in_range: boolean | null;
  /** `POST /api/harnesses/update` only: why this item failed. */
  error?: string;
}

/** The body of a harness update. `version`: `latest` (the default) or `x.y.z`. */
export interface HarnessUpdateOptions {
  version?: string;
  /** Update also while agents on the harness run. */
  force?: boolean;
}

/** What an agent start does outside the range (`TERRARIUM_HARNESS_VERSION_POLICY`). */
export type HarnessVersionPolicy = "warn" | "refuse";

export interface HarnessReport {
  policy: HarnessVersionPolicy;
  harnesses: HarnessVersion[];
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
  /** Replaces the harness's default system prompt. Terrarium's protocol prompt is added after it. Absent: the harness default. At most 32 KiB. */
  system_prompt?: string | null;
  roles?: Role[];
  policy?: AgentPolicy;
  restart?: RestartPolicy;
  /** Start a new session, not a resume, when the prompt changed (Codex). The daemon leaves it out when true. */
  new_session_on_prompt_change?: boolean;
}

/**
 * The body of `POST /api/agents` (docs/api.md §6.1, §21). With `template`,
 * only `name` and `project` are needed: each field given overrides the
 * template's field, `policy` merges per key, and `null` clears
 * `subscription`, `model`, `prompt`, or `system_prompt`.
 */
export type AgentCreate =
  | AgentDef
  | (Pick<AgentDef, "name" | "project"> &
      Partial<Omit<AgentDef, "name" | "project" | "subscription" | "model" | "prompt">> & {
        /** A template key: a name, `global:<name>`, `<project>/<name>`, or a ULID. */
        template: string;
        subscription?: string | null;
        model?: string | null;
        prompt?: string | null;
      });

/** `PATCH /api/agents/{id}`. Name, project, and roles do not change here. `null` clears subscription, model, prompt, or system_prompt; an absent field does not change. A new system_prompt applies at the next start. */
export type AgentPatch = Partial<Pick<AgentDef, "harness" | "policy" | "restart">> & {
  subscription?: string | null;
  model?: string | null;
  prompt?: string | null;
  system_prompt?: string | null;
};

/** `subscription_detail` of `GET /api/agents/{id}`: the agent's subscription, when it exists. */
export interface AgentSubscriptionDetail {
  name: string;
  provider: string;
  plan?: string;
  login_status: LoginStatus;
  harnesses: Harness[];
  /** The model the agent's harness gets when the agent sets no `model`. */
  default_model?: string;
}

/** docs/api.md §6. */
export interface Agent {
  id: string;
  name: string;
  project: string;
  harness: Harness;
  subscription?: string;
  model?: string;
  prompt?: string;
  /** Replaces the harness's default system prompt. Absent or null: the harness default. */
  system_prompt?: string | null;
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
  /** Only in `GET /api/agents/{id}`: the subscription, when it exists. */
  subscription_detail?: AgentSubscriptionDetail;
  /** The template the agent was made from. The values were copied at the create; there is no live link. */
  template?: TemplateRef;
  /** The number of open waits. Absent when 0. */
  waits?: number;
  /** A short summary of the open waits, for example `agent demo/w1 (done); process build on box`. Absent when none. */
  waiting_on?: string;
  /** Only in `GET /api/agents/{id}`: the open waits. */
  open_waits?: Wait[];
}

// ----- Waits and watches -----

export type WaitKind = "agent" | "process" | "time" | "card";
export const WAIT_KINDS: WaitKind[] = ["agent", "process", "time", "card"];
export type WaitState = "open" | "resolved" | "timed_out" | "cancelled" | "failed";
/** What an agent wait waits for. `done` is idle with no open waits. */
export type WaitUntil = "done" | "idle" | "blocked" | "any_change";

export interface AgentWaitSpec {
  agent: string;
  agent_name?: string;
  until: WaitUntil;
  from_status?: string;
}
export interface ProcessWaitSpec {
  target: string;
  /** The exec session ID. */
  session: string;
  name?: string;
  command?: string;
}
export interface TimeWaitSpec {
  due_at: string;
}
export interface CardWaitSpec {
  card: string;
  column: string;
}

/** The end of a process run. `stdout_tail` and `stderr_tail` are untrusted text: show them as plain text only. */
export interface ProcessWaitResult {
  exit_code: number | null;
  signal: string | null;
  reason: string;
  duration_secs?: number;
  stdout_tail?: string;
  stderr_tail?: string;
}

/** One wait of an agent: it holds the agent in `waiting` until it closes. */
export interface Wait {
  id: string;
  /** The waiter's ULID. */
  agent: string;
  agent_name: string;
  project: string;
  kind: WaitKind;
  spec: AgentWaitSpec | ProcessWaitSpec | TimeWaitSpec | CardWaitSpec | Record<string, unknown>;
  summary: string;
  state: WaitState;
  reason?: string | null;
  /** `operator` or `agent:<ulid>`. */
  created_by: string;
  created_at: string;
  timeout_at?: string | null;
  resolved_at?: string | null;
  /** A process run: `ProcessWaitResult`. Other kinds: `{outcome}`. */
  result?: ProcessWaitResult | { outcome?: string; [k: string]: unknown } | null;
}

/** `GET /api/waits/{id}/log`: the output tail of a process wait. `output` is untrusted text. */
export interface WaitLog {
  wait_id: string;
  session_id: string;
  target: string;
  running: boolean;
  output: string;
  end?: Record<string, unknown> | null;
}

/** The statuses a watch can fire on. The API also takes `busy` for `working`. */
export type WatchOn = "idle" | "waiting" | "working" | "blocked" | "error" | "stopped";
export const WATCH_ON: WatchOn[] = ["idle", "waiting", "blocked", "error", "stopped", "working"];

/** A watch: the watcher gets mail when the watched agent goes to one of the `on` statuses. */
export interface Watch {
  id: string;
  /** A ULID, or `operator`. */
  watcher: string;
  watcher_name: string;
  /** The watched agent's ULID. */
  agent: string;
  agent_name: string;
  project: string;
  on: WatchOn[];
  once: boolean;
  debounce: boolean;
  created_by: string;
  created_at: string;
  fired: number;
  last_fired_at?: string | null;
}

/** `POST /api/agents/{id}/watches`: `{id}` is the watched agent. */
export interface NewWatch {
  on: (WatchOn | "busy")[];
  once?: boolean;
  debounce?: boolean;
}

/** `data` of a `wait` event. */
export interface WaitEventData {
  wait_id: string;
  change: "opened" | "resolved" | "timed_out" | "cancelled" | "failed";
  kind: WaitKind;
  summary: string;
  state: WaitState;
}

/** `data` of a `watch` event. */
export interface WatchEventData {
  watch_id: string;
  change: "created" | "fired" | "removed";
  on: WatchOn[];
  status?: string;
}

/** `GET /api/agents/{id}/screen` (docs/api.md §6.5): the harness screen as plain text. */
export interface AgentScreen {
  /** `project/name`. */
  agent: string;
  agent_id: string;
  status: AgentStatus;
  rows: number;
  cols: number;
  /** The screen rows, no escape codes, no trailing blanks. */
  text: string;
  cursor?: { row: number; col: number };
  /** RFC 3339: when the daemon took the snapshot. */
  at: string;
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
  /** docs/api.md §6 (transcripts): the harness transcript is readable (a path or true). Absent on an older daemon. */
  transcript?: boolean | string | null;
  /** docs/api.md §6 (transcripts): how many transcript blocks the session has. */
  blocks?: number;
  /** docs/api.md §6 (transcripts): a short title, from the first user turn. */
  title?: string | null;
  /** docs/api.md §6 (transcripts): the harness runs this session now, so a turn can go to it. */
  active?: boolean;
}

// ----- Session transcripts (docs/api.md §6) -----

export type BlockRole = "user" | "assistant" | "system";
export type BlockKind = "text" | "thinking" | "tool_call" | "tool_result" | "system";
/** Who wrote a user turn: the operator (dashboard or CLI), a mail pointer, the daemon, or keys typed in the terminal. */
export type BlockOrigin = "operator" | "agent_mail_pointer" | "system" | "user";

export interface BlockUsage {
  input: number;
  output: number;
  cache_read: number;
  cache_write: number;
}

/** One block of a harness conversation, read from the harness's own transcript file. */
export interface Block {
  /** Stable across `update` frames. */
  id: string;
  /** The terrarium session ID. */
  session: string;
  /** The position in the session's transcript, from 1, without gaps. `?after=` takes it. */
  seq: number;
  at: string | null;
  role: BlockRole;
  kind: BlockKind;
  text?: string;
  tool?: { id: string; name: string; input: unknown; summary: string };
  result?: { tool_id: string; output: string; truncated?: boolean; is_error: boolean };
  /** Thinking that the harness did not keep (only a signature). */
  redacted?: boolean;
  origin?: BlockOrigin;
  model?: string;
  usage?: BlockUsage;
  source: { harness: Harness; file: string; line: number };
}

/** `GET /api/agents/{id}/sessions/{session}/transcript?after=&limit=`. */
export interface TranscriptPage {
  session: string;
  harness: Harness;
  active: boolean;
  blocks: Block[];
  /** Pass as `after` for the next page. */
  next_after: number;
  /** True when this page reaches the end of the transcript as it is now. */
  complete: boolean;
}

/** One frame of `GET .../transcript/stream?after=` (SSE). */
export type TranscriptFrame =
  | { type: "block"; block: Block }
  /** A block that grew (the same `seq` and `id`, more text). */
  | { type: "update"; block: Block }
  | { type: "status"; status: string };

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
  "warning",
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
  // Waits: `{wait_id, change, kind, summary, state}`.
  "wait",
  // Watches: `{watch_id, change, on, status?}`.
  "watch",
  // docs/api.md §4: a harness update in the container.
  "harness",
  // docs/api.md §19: `{change, schedule, run?, status?}`.
  "schedule",
  // docs/api.md §20: `{change, source, name, key, kind, enabled}`.
  "event_source",
  // docs/api.md §21: `{change, template, name, key, revision}`.
  "template",
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
  /** Kind-specific payload. `status`: `{status, from, reason, waits?, waiting_on?}`. `card`: `{card_id, change, ...}`. */
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
/** `system` is the daemon (quota alerts, blocked-card notices, loop-guard notices). It only sends: `to_kind` is never `system`. */
export type AddressKind = "operator" | "system" | "project" | "agent";

export interface Message {
  id: string;
  thread_id: string;
  /** Stored address: `operator`, `system`, `project:<name>`, or the agent ULID. */
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
  /** Who read the message first: `operator`, or an agent ULID. Absent until the first read or reply. */
  read_by?: string;
  read_by_name?: string;
  /** The loop guard paused the thread (docs/api.md §6.2). Absent when false. */
  thread_paused?: boolean;
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
/** `api_key`: a stored Z.ai key; no harness login to check. */
export type LoginStatus = "logged_in" | "expiring" | "expired" | "missing" | "unknown" | "api_key";
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
  /** The money left, when the window is a credit balance (`openrouter-credits`). */
  balance?: { total: number; used: number; remaining: number; currency: string };
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
  /**
   * The model each harness gets on this subscription when the agent sets no
   * `model`. Only harnesses where the daemon picks it (pi, Claude Code on
   * Z.ai). Left out when there is none, and by older daemons.
   */
  default_models?: Partial<Record<Harness, string>>;
}

/** Where `GET /api/subscriptions/{name}/models` found the list: the provider's `/models`, the harness login's list, pi's catalogue, or the default table. */
export type ModelSource = "provider" | "plan" | "pi" | "fallback";

export interface ModelInfo {
  id: string;
  name?: string;
  /** The context window in tokens. */
  context?: number;
}

/** `GET /api/subscriptions/{name}/models` (docs/api.md §16.3). */
export interface ModelList {
  models: ModelInfo[];
  source: ModelSource;
  fetched_at: string;
  /** Why an earlier source gave no list. */
  notes?: string[];
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
  /** `zai` only: `coding` (GLM Coding Plan, the default) or `api` (pay as you go). */
  plan?: string;
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

export type TargetKind = "host" | "ssh" | "docker" | "vmlab" | "vnc" | "rdp" | "desktop";
/** docs/api.md §17.10: the display server of a `docker` or `desktop` target. */
export type DisplayProtocol = "x11" | "wayland" | "auto";
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
  /** `vmlab`: a lab dir on the host that the operator wrote (docs/api.md §17.2). */
  lab?: string;
  machine?: string;
  /** `vmlab`: a repo of the target's project; the host makes a git worktree of it on `up`. */
  lab_repo?: string;
  /** `vmlab` with `lab_repo`: a branch, a tag, or a commit. Default: the repo's default branch, else `main`. */
  lab_ref?: string;
  /** `vmlab` with `lab_repo` or `lab_from_workspace`: the relative dir that holds `vmlab.wcl`. Default: the root. */
  lab_path?: string;
  /** `vmlab`: the lab is in the workspace of each holder; each holder runs its own instance. */
  lab_from_workspace?: boolean;
  /** `vnc` and `rdp`: the user name to log in with. */
  username?: string;
  /** `rdp`: the Windows domain of the user. */
  domain?: string;
  /** `docker` and `desktop`: the display server (docs/api.md §17.10). */
  display?: DisplayProtocol;
  /** `docker` and `desktop`: the X display or the Wayland socket. */
  display_name?: string;
  /** The target has a screen (docs/api.md §17.10). */
  has_display?: boolean;
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
  /** What runs now (docs/api.md §17.8). `{}` when nothing runs; absent on a daemon from before mode 1. */
  runtime?: TargetRuntime;
}

/** docs/api.md §17.8: what runs on a target now. */
export interface TargetRuntime {
  /** The agent workspaces (`<project>/<agent>`) in the container or the lab instance. Each is at `/work/<project>/<agent>`. */
  attached?: string[];
  /** A `lab_repo` target: the lab dir of its instance on the host. */
  lab?: string;
  /** A `lab_repo` target: the commit of its worktree. */
  head?: string;
  /** A `lab_from_workspace` target: one instance for each holder, by the agent ULID. */
  instances?: Record<string, LabInstance>;
}

/** One lab instance of a `lab_from_workspace` target (docs/api.md §17.8). */
export interface LabInstance {
  /** `project/name` of the holder. */
  agent_name: string;
  /** The lab dir of the instance on the host. */
  lab?: string;
  state: TargetState;
  attached?: string[];
}

/** `details` of the `409 conflict` of `POST /api/targets` past `TERRARIUM_MAX_TARGETS_PER_PROJECT` (docs/api.md §17.2). */
export interface ProjectCapDetails {
  project: string;
  targets: number;
  max: number;
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
  lab_repo?: string;
  lab_ref?: string;
  lab_path?: string;
  lab_from_workspace?: boolean;
  username?: string;
  domain?: string;
  display?: DisplayProtocol;
  display_name?: string;
  ephemeral?: boolean;
  keep?: boolean;
  snapshot_before_task?: boolean;
}

// ----- Target displays (docs/api.md §17.10) -----

/** A display operation, as in the path `POST .../display/{op}`. */
export type DisplayOpName =
  | "info"
  | "screenshot"
  | "move"
  | "click"
  | "drag"
  | "scroll"
  | "keys"
  | "type"
  | "ocr"
  | "find"
  | "record";

/** A rectangle on the screen, in pixels. */
export interface DisplayRegion {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** The body of `POST .../display/screenshot`. */
export interface ScreenshotOpts {
  scale?: number;
  region?: DisplayRegion;
  agent?: string;
}

/** `GET /api/targets/{name}/display/info`. */
export interface DisplayInfo {
  target: string;
  kind: TargetKind;
  backend: string;
  width: number;
  height: number;
  ops: DisplayOpName[];
  /** The OCR engine: the backend's own, `tesseract`, or `null`. */
  ocr: string | null;
  input: { allowed: boolean; reason?: string | null };
}

/** `POST .../display/screenshot?format=json`. */
export interface DisplayShot {
  target: string;
  op: "screenshot";
  format: "png";
  width: number;
  height: number;
  screen: { width: number; height: number };
  region?: DisplayRegion | null;
  scale?: number | null;
  bytes: number;
  sha256: string;
  png_base64: string;
}

/** The answer of an input operation. */
export interface DisplayOpResult {
  target: string;
  op: DisplayOpName;
  ok?: boolean;
  [key: string]: unknown;
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
  /** `snapshot` and `restore`: the machine, or the whole lab. */
  scope?: SnapshotScope;
  result?: unknown;
  /** docs/api.md §17.3: an `up` of a running target that could not attach a holder's workspace. */
  attach_error?: string;
}

/** The body of a lifecycle verb (docs/api.md §17.3). */
export interface TargetVerbBody {
  /** A `lab_from_workspace` target: the holder (an agent key) whose instance the verb acts on. */
  agent?: string;
  name?: string;
  destroy?: boolean;
  /** `snapshot` and `restore`: the target's machine, or the whole lab. Default: `machine` when the target has one. */
  scope?: SnapshotScope;
  /** `restore`: restore a dev machine whose workspace is halted, and drop the guest copies. */
  discard_guest_changes?: boolean;
}

/** Which machines a snapshot verb acts on (docs/api.md §17.3): `--vm <machine>`, or the whole lab. */
export type SnapshotScope = "machine" | "lab";

/** One snapshot as the host reports it, merged over the machines (`GET /api/targets/{name}/snapshots`). */
export interface TargetSnapshot {
  name: string;
  /** `online` or `offline`. */
  kind?: string;
  /** The capture time (UTC), as vmlab prints it. */
  taken?: string;
  /** The machines that have a snapshot with this name. */
  machines: string[];
  /** Terrarium took it: the name is in the target's `snapshots`. */
  managed: boolean;
}

/** `GET /api/targets/{name}/snapshots` (docs/api.md §17.3). */
export interface TargetSnapshotList {
  target: string;
  scope: SnapshotScope;
  /** The machines the list read. */
  machines: string[];
  snapshots: TargetSnapshot[];
  /** Stored names that the host no longer has; the list removed them from the target's `snapshots`. */
  dropped: string[];
  /** The `lab` scope only: false when the lab does not run (vmlab then names no machines). */
  lab_running?: boolean;
  /** A `lab_from_workspace` target: the holder (`project/name`) whose instance. */
  agent?: string;
}

/** The query of the snapshot list and delete. */
export interface SnapshotQuery {
  scope?: SnapshotScope;
  /** A `lab_from_workspace` target: the holder whose instance. */
  agent?: string;
  /** Delete: `terrarium-clean` while `snapshot_before_task` is on. */
  force?: boolean;
}

/** `DELETE /api/targets/{name}/snapshots/{snap}` (docs/api.md §17.3). */
export interface SnapshotDeleted {
  target: string;
  verb: "snapshot_delete";
  snapshot: string;
  scope: SnapshotScope;
  machines: string[];
  agent?: string;
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
  /** Target displays (docs/api.md §17.10). Absent from an older host agent: off. */
  display?: DisplayCapability;
}

/** `terrarium_core::protocol::DisplayCapability`. */
export interface DisplayCapability {
  /** The host agent's own config opts in its desktop (`[display] enabled = true`). */
  enabled: boolean;
  allowed_displays?: string[];
  tesseract?: boolean;
  tools?: string[];
  drivers?: string[];
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
  summary?: { os: string; arch: string; docker: boolean; vmlab: boolean; display?: boolean };
  last_seen?: string | null;
  os?: string | null;
  arch?: string | null;
  version?: string | null;
  max_sessions?: number;
  /** docs/api.md §17.3: the docker and vmlab targets (and lab instances) that may run at one time. Absent: an older host agent, no limit. */
  max_running_targets?: number;
  /** The host path of the work root, when the daemon reports it (it does not yet). */
  work_dir?: string;
  /** The host dir of the lab instances, when the daemon reports it (it does not yet). */
  instances_dir?: string;
  connected_at?: string;
  created_at?: string;
  created_by?: string;
}

/** `POST /api/hosts` (`201`): the host and its token. The token shows once, here only. */
export type NewHostToken = Host & {
  token: string;
  /** `host` (a long-lived `thk_` token) or `enrol` (a one-time `the_` token, 10 minutes). */
  token_kind?: "host" | "enrol";
  /** For an enrol token: when it stops working. */
  token_expires_at?: string;
  /** For an enrol token, when the daemon knows its URL: the line that installs the host agent (docs/api.md §17.9). */
  install_command?: string;
};

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
  /** When the cookie ends, RFC 3339. A request past half the life renews the cookie, so the session can last longer. */
  expires_at: string;
  /** The same time as a Unix time, in seconds. */
  exp?: number;
  /** `browser` for the cookie. */
  kind?: string;
}

/**
 * The answer of `GET /api/session` (docs/api.md §1.1): the session of this
 * request. The dashboard calls it at load, so it keeps no copy of the CSRF
 * value. A renewed cookie keeps the same CSRF value.
 */
export interface SessionInfo {
  /** `browser` (the cookie), `cli`, `token` (the static bearer token), or `local` (the Unix socket). */
  kind: "browser" | "cli" | "token" | "local";
  /** The CSRF value of the cookie session (`browser` only). */
  csrf?: string;
  exp?: number;
  expires_at?: string;
  /** Is an operator password set? */
  password_set: boolean;
}

/** The body of `PUT /api/auth/password`. A session must send `current` when a password is set. */
export interface PasswordChange {
  current?: string;
  new: string;
}

// ----- Schedules (docs/api.md §19) -----

/**
 * When a schedule fires. `cron`: a 5-field expression in `tz` (an IANA
 * zone, default `UTC`). `interval`: a duration such as `15m`, `1h`, or
 * `1h30m`. `at`: once, at an RFC 3339 time.
 */
export type ScheduleSpec = { kind: "cron"; expr: string; tz: string } | { kind: "interval"; every: string } | { kind: "at"; at: string } | TriggerSpec;
export type ScheduleSpecKind = ScheduleSpec["kind"];
/** The specs of a time schedule; `on` is a trigger. */
export type TimeSpec = Exclude<ScheduleSpec, TriggerSpec>;

/** The compare of a `match` rule. `in` takes a list. */
export type MatchOp = "eq" | "ne" | "glob" | "in";

/** One `match` rule of a trigger (docs/api.md §19.4). */
export interface MatchRule {
  /** `project`, `kind`, `actor`, `summary`, or `data.<dotted path>`. */
  path: string;
  /** Default `eq`. */
  op?: MatchOp;
  /** A string for `eq`, `ne`, and `glob`; a list for `in`. A number compares as its JSON text. */
  value: string | number | boolean | (string | number | boolean)[];
}

/** One run for each rendered `key` in `window` (`90s`, `10m`, `1h`; 1 s to 7 d). */
export interface TriggerDedupe {
  key: string;
  window: string;
}

/** After `after` matches in a row for one key, mail `to` (default `operator`) once instead of the action. */
export interface TriggerEscalate {
  key: string;
  after: number;
  /** Kind globs whose events with the same key set the count to zero. */
  reset_on?: string[];
  to?: string;
}

/** A trigger: an event of the log that matches starts a run (docs/api.md §19.4). */
export interface TriggerSpec {
  kind: "on";
  /** Kind globs: `*` any text, `?` one character. */
  kinds: string[];
  match?: MatchRule[];
  dedupe?: TriggerDedupe;
  escalate?: TriggerEscalate;
  /** 1-600, default 30. */
  max_per_hour?: number;
}

export type AgentVerb = "start" | "stop" | "restart";

/** What a schedule does when it fires. */
export type ScheduleAction =
  /** `to`: `operator`, `project:<name>`, or `project/agent`. `thread`: post every run on this thread. */
  | { type: "mail"; to: string; subject: string; body: string; thread?: string }
  | { type: "turn"; agent: string; text: string; start_if_stopped?: boolean }
  /** `timeout_secs`: default 300, at most 3600. */
  | { type: "exec"; target: string; cmd: string[]; cwd?: string; timeout_secs?: number }
  | { type: "agent"; agent: string; verb: AgentVerb }
  | { type: "probe"; subscription: string }
  /**
   * A card on the board of the schedule's project. `open`: a new card with
   * the label `trigger:<key>` in `column` (default `backlog`), or a comment
   * on the open card with that label. `close`: move it to `column`
   * (default `done`). A global schedule cannot have it.
   */
  | { type: "card"; op: CardOp; key: string; title?: string; body?: string; column?: string };
export type ScheduleActionType = ScheduleAction["type"];
export type CardOp = "open" | "close";
/** The actions a trigger may run. `turn`, `agent`, and `probe` on a trigger are `422`. */
export type TriggerActionType = Extract<ScheduleActionType, "mail" | "exec" | "card">;

export type CatchUp = "skip" | "once";

/** The end of a run. A run that has not ended yet is `running`. */
export type RunStatus = "ok" | "failed" | "skipped";

export interface ScheduleLastRun {
  at: string;
  status: RunStatus;
  summary: string;
  run_id: string;
}

export interface Schedule {
  id: string;
  name: string;
  /** Null: a global schedule (sysadmin only). */
  project: string | null;
  enabled: boolean;
  spec: ScheduleSpec;
  action: ScheduleAction;
  catch_up: CatchUp;
  /** How many runs may go on at once. */
  max_concurrency?: number;
  /** `operator` or `agent:<ulid>`. */
  created_by: string;
  /** The actor the runs act as; the daemon checks its rights at each run. */
  run_as: string;
  /** Null when disabled, or when a one-shot `at` schedule has run. */
  next_run_at: string | null;
  last_run: ScheduleLastRun | null;
  runs_count: number;
  /** `api`, `file`, or `fleet:<name>`. A `file` schedule changes only in its file (`409 file_managed`). */
  source: string;
  /** Rights checks in a row that skipped a run. */
  consecutive_skips?: number;
  /** Why the daemon disabled the schedule, when it did. */
  disabled_reason?: string;
  created_at: string;
  updated_at: string;
}

/** `POST /api/schedules`. No `project` (or null): a global schedule. */
export interface NewSchedule {
  name: string;
  project?: string | null;
  enabled?: boolean;
  spec: ScheduleSpec;
  action: ScheduleAction;
  catch_up?: CatchUp;
}

/** `PATCH /api/schedules/{id}`: a `spec` or an `action` replaces the whole object. The scope does not change. */
export type SchedulePatch = Partial<Omit<NewSchedule, "project">>;

export interface ScheduleQuery {
  project?: string;
  enabled?: boolean;
  /** `time`: only the time schedules; `trigger`: only the triggers (spec `on`). */
  kind?: "time" | "trigger";
}

/** `cron` is the timer (each time spec); `event` is a trigger run. */
export type RunTrigger = "cron" | "manual" | "catch_up" | "event";

/** The event that started a trigger run, while the log has it. */
export interface RunEvent {
  seq: number;
  kind: string;
  summary: string;
}

/** `GET /api/schedules/{id}/runs`: newest first. */
export interface ScheduleRun {
  id: string;
  schedule: string;
  started_at: string;
  /** Null while the run goes on. */
  finished_at: string | null;
  /** `running` until the run ends. */
  status: RunStatus | "running";
  trigger: RunTrigger;
  summary: string;
  /**
   * A JSON value. Exec: `{target, cmd, exit_code, end, stdout, stderr}` (the
   * tails). Mail: the message ID. Turn: the delivery. Agent: the new
   * status. Probe: the windows.
   */
  output: unknown;
  error: string | null;
  /** Who triggered the run: the `run_as`, the caller of a run now, or the actor of the event of a trigger run. */
  actor?: string;
  /** The `seq` of the event that started a trigger run; null for the other runs. */
  event_seq?: number | null;
  /** That event, while the log has it. Absent for the other runs. */
  event?: RunEvent;
}

export type ScheduleChange = "created" | "updated" | "deleted" | "enabled" | "disabled" | "run_started" | "run_finished";

/**
 * The data of a `schedule` event. The daemon sends IDs: `schedule` is the
 * schedule ID, `run` the run ID; the view reads the rest again.
 */
export interface ScheduleEventData {
  change: ScheduleChange;
  schedule: string;
  name: string;
  project: string | null;
  run?: string;
  status?: RunStatus | "running";
  trigger?: RunTrigger;
}

// ----- Event sources (docs/api.md §20) -----

/** The sender of a source: it sets the signature check and how a body becomes an event. */
export type EventSourceKind = "github" | "forgejo" | "generic";

/** One webhook sender. The secret is never in it, except in the answer of a create or a rotate. */
export interface EventSource {
  id: string;
  name: string;
  /** Null: a global source (sysadmin only); its events have no project. */
  project: string | null;
  kind: EventSourceKind;
  enabled: boolean;
  created_by: string;
  /** The last request with a good signature. */
  last_seen: string | null;
  /** The time and the reason of the last failed request. A good request clears it. */
  last_error: string | null;
  /** The events that the source added to the log. */
  received: number;
  created_at: string;
  updated_at: string;
  /** `/hooks/{project}/{name}`; `_` is the project part of a global source. */
  hook_path: string;
  /** The URL to give the sender. Null when the ingest listener is off. */
  hook_url: string | null;
  /** Why `hook_url` may be wrong or is null. */
  hook_note?: string;
  /** Short setup steps for the sender: the content type, the secret, and which events to select. */
  setup: string[];
}

/** The answer of `POST /api/event-sources` and `POST .../rotate`: the source and its secret, shown this one time. */
export interface EventSourceWithSecret extends EventSource {
  secret: string;
}

/** `POST /api/event-sources`. No `project` (or null): a global source. */
export interface NewEventSource {
  name: string;
  kind: EventSourceKind;
  project?: string | null;
  enabled?: boolean;
}

/** `PATCH /api/event-sources/{id}`. A new name changes the hook URL. */
export interface EventSourcePatch {
  name?: string;
  enabled?: boolean;
}

// ----- Agent templates (docs/api.md §21) -----

/** An agent definition without `name` and `project`. The daemon normalizes it: policy and restart get their defaults. */
export type TemplateSpec = Omit<AgentDef, "name" | "project">;

/** A stored agent template. */
export interface AgentTemplate {
  id: string;
  name: string;
  /** Null: a global template. */
  project: string | null;
  /** Empty when none. */
  description: string;
  spec: TemplateSpec;
  /** 1 at the create; one more at each change. */
  revision: number;
  created_by: string;
  /** `api`, `file` (a project file), or `fleet:<name>`. */
  source: Source | `fleet:${string}`;
  created_at: string;
  updated_at: string;
}

/** The template of an agent, as the agent records it. */
export interface TemplateRef {
  id: string;
  name: string;
  project: string | null;
  revision: number;
}

/** `POST /api/templates`. No `project`: the caller's project (the operator: global). `null`: global. */
export interface NewTemplate {
  name: string;
  project?: string | null;
  description?: string;
  spec: TemplateSpec;
}

/** `PATCH /api/templates/{key}`. `spec` replaces the whole spec. */
export interface TemplatePatch {
  name?: string;
  description?: string;
  spec?: TemplateSpec;
}

/** `GET /api/templates/{key}/resolve?project=`: the template and the agent fields it gives in that project. */
export interface TemplateResolved {
  template: TemplateRef;
  agent: Omit<AgentDef, "name">;
}

/** `POST /api/templates/from-agent/{agent}`. No `project`: the agent's project. `null`: global. */
export interface TemplateFromAgent {
  name: string;
  project?: string | null;
  description?: string;
}
