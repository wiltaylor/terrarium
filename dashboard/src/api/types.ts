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

/** The queue of a queued turn. */
export type QueuedSource = "store" | "supervisor" | "runner" | "hold";

/** One untouched queued turn (`GET /api/agents/{id}/queued`). */
export interface QueuedTurn {
  id: number | string;
  /** Where the turn waits. Live queue IDs are opaque strings. */
  source: QueuedSource;
  /** The turn as it will be typed, with its tag (`[User] ...`). */
  text: string;
  /** The text after the tag: what the composer gets for an edit. */
  body: string;
  created_at: string;
}

/** Roles above `member`. The daemon drops `member`; it is implicit. */
export type Role = "sysadmin" | "project_admin" | "member" | "architect";
export const GRANTABLE_ROLES = ["project_admin", "sysadmin", "architect"] as const;
/** The label of a role in the dashboard: `architect` is the system architect (TER-47). */
export const roleLabel = (r: string): string => (r === "architect" ? "system architect" : r);

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
  /** The global idle stop of an agent: a duration such as `10m`, or `off`. An agent's `idle_stop` overrides it. */
  idle_stop?: string;
  /** The global `compact_on_idle` (`TERRARIUM_COMPACT_ON_IDLE`). An agent's own value overrides it. */
  compact_on_idle?: boolean;
  /** The global `compact_max_age`: `off`, or a duration such as `24h`. An agent's own value overrides it. */
  compact_max_age?: string;
  /** The harnesses whose agents the daemon can compact. */
  compact_harnesses?: Harness[];
}

/** `GET /api/system/version` (docs/api.md §4): the build of the daemon. Any caller may read it. */
export interface SystemVersion {
  /** The full version: `<cargo version>+<git ref>`, with `-dirty` for a tree with changes, or `+unknown`. */
  version: string;
  /** The crate version, for example `0.1.0`. */
  cargo_version: string;
  /** The short commit of the build, or `unknown`. */
  git_ref: string;
  /** The build tree had changes to tracked files. */
  dirty: boolean;
  /** The build time, RFC 3339 UTC. */
  build_time: string;
}

/** An image as the updater knows it (docs/api.md §34.6). */
export interface UpdaterImage {
  /** `ghcr.io/...@sha256:...`, or a local tag (the image of a source deploy). */
  reference: string;
  /** The image ID. */
  id: string;
  digest?: string;
  /** The `org.opencontainers.image.version` label. */
  version?: string;
}

export type UpdateOutcome = "running" | "succeeded" | "current" | "failed" | "rolled_back" | "rollback_failed";
export type UpdateStepState = "running" | "ok" | "failed" | "skipped";

/** One step of an update or a rollback. */
export interface UpdateStep {
  name: string;
  state: UpdateStepState;
  detail: string;
  at: string;
}

/** One update or rollback of the updater. */
export interface UpdateRun {
  id: string;
  kind: "update" | "rollback";
  actor: string;
  target: string;
  from: UpdaterImage | null;
  to: UpdaterImage | null;
  backup: string | null;
  outcome: UpdateOutcome;
  error: string | null;
  steps: UpdateStep[];
  started_at: string;
  ended_at: string | null;
}

/** A GitHub release of Terrarium. */
export interface TerrariumRelease {
  version: string;
  tag: string;
  name: string;
  prerelease: boolean;
  published_at: string | null;
  /** Markdown. */
  notes: string;
  url: string;
}

/** `GET /api/system/update` (docs/api.md §34): the operator and sysadmins. */
export interface SelfUpdateReport {
  current: SystemVersion;
  /** `TERRARIUM_SELF_UPDATE`. */
  policy: "off" | "notify" | "auto";
  /** `TERRARIUM_UPDATE_CHANNEL`. */
  channel: "stable" | "pre";
  latest: TerrariumRelease | null;
  update_available: boolean;
  checked_at: string | null;
  check_error: string | null;
  updater: {
    reachable: boolean;
    socket: string;
    error: string | null;
    status: {
      updater_version: string;
      repository: string;
      verify: string;
      current: UpdaterImage | null;
      current_error?: string;
      previous: UpdaterImage | null;
      running: UpdateRun | null;
      history: UpdateRun[];
    } | null;
  };
  last_result: UpdateRun | null;
  maintenance: { key: string; event: string; title: string; start: string; end: string } | null;
  drain: { on: boolean; since: string | null; by: string | null; busy: string[] };
}

/** `GET /api/harnesses` (docs/api.md §4): one harness against its pin. */
export interface HarnessVersion {
  harness: Harness;
  package: string;
  /** The base version: the version of the first install (harnesses.toml). It is not a pin: the runtime pin is `auto_update.pinned_version`. */
  base: string;
  /** Deprecated: the same value as `base`, for older clients. It is not the runtime pin. */
  pinned: string;
  /** What `<binary> --version` reported; null when it did not answer. */
  installed: string | null;
  error?: string;
  checked_at: string | null;
  /** `?latest=true` only: the latest release on npm; null when npm did not answer (`latest_error` says why). */
  latest?: HarnessLatest | null;
  /** `?latest=true` only: the bump from `installed` (else `base`) to `latest` (`scripts/harness.py`); null when `latest` is null. */
  update?: HarnessUpdateKind | null;
  /** `?latest=true` only, when `latest` is null: why. */
  latest_error?: string;
  /** The active version dir in the tools dir: the version the next agent start runs. Absent without a tools dir. */
  active?: string;
  /** The complete version dirs in the tools dir, oldest first. Absent without a tools dir. */
  versions?: string[];
  /** The live agents on the harness and the version each runs. Absent when none runs. */
  agents?: HarnessAgent[];
  /** The update policy and the last auto-update check (docs/api.md §4, "Auto-update and pins"). */
  auto_update?: HarnessAutoUpdate;
}

/** `auto`: the background check installs each newer npm release. `pinned`: it installs nothing. */
export type HarnessUpdatePolicy = "auto" | "pinned";

/** The result of one harness in an auto-update check. */
export type HarnessCheckResult = "current" | "updated" | "pinned" | "failed" | "skipped";

/** The `auto_update` object of a row of `GET /api/harnesses`. */
export interface HarnessAutoUpdate {
  policy: HarnessUpdatePolicy;
  /** The version of a `pinned` policy; null for `auto`. */
  pinned_version: string | null;
  /** The rollback target: the version before the last switch (on disk), else the newest version dir under the active one. */
  previous: string | null;
  /** The latest npm release at the last check. */
  latest: string | null;
  checked_at: string | null;
  result: HarnessCheckResult | null;
  /** One line about the last check. */
  detail: string | null;
  /** The release notes, when the daemon knows where they are. */
  changelog: string | null;
}

/** `POST /api/harnesses/{name}/pin` and `.../unpin`. */
export interface HarnessPinResult {
  harness: string;
  policy: HarnessUpdatePolicy;
  pinned_version: string | null;
  /** The active version before. */
  from: string | null;
  /** The active version after. */
  to: string | null;
  /** npm's output, when the pin installed the version. */
  output?: string;
  agents_on_old_version: HarnessAgent[];
  note?: string;
  removed?: string[];
}

/** One harness in `POST /api/harnesses/check`. */
export interface HarnessCheckItem {
  harness: string;
  result: HarnessCheckResult;
  detail: string;
  from?: string;
  to?: string;
  latest: string | null;
  /** True when the check sent a mail to the operator. */
  mailed: boolean;
}

/** `POST /api/harnesses/check`. */
export interface HarnessCheckReport {
  checked_at: string;
  results: HarnessCheckItem[];
}

/** A live agent and the harness version it runs (`GET /api/harnesses`, and the update answer). */
export interface HarnessAgent {
  /** `project/name`. */
  agent: string;
  id: string;
  /** The version dir its launch resolved; null when not known (an agent that started before the version dirs). */
  version: string | null;
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
  /** True when the version dir was complete on disk, so npm did not run. */
  reused: boolean;
  /** The live agents that run another version than `to`. Each keeps its version until its next restart. */
  agents_on_old_version: HarnessAgent[];
  /** What happens to `agents_on_old_version`, in one line. */
  note?: string;
  /** The version dirs of this harness that the GC after the update removed. */
  removed?: string[];
  /** `POST /api/harnesses/update` only: why this item failed. */
  error?: string;
}

/** The body of a harness update. `version`: `latest` (the default) or `x.y.z`. */
export interface HarnessUpdateOptions {
  version?: string;
  /** Install the version again, also when its version dir is on disk. Running agents never stop an update. */
  force?: boolean;
}

export interface HarnessReport {
  harnesses: HarnessVersion[];
  /** `TERRARIUM_HARNESS_AUTO_UPDATE` (`daily`, `off`, or a duration) and the next background check. */
  auto_update?: { schedule: string; next_check_at: string | null };
}

/** docs/api.md §5. */
export interface Project {
  home_board_columns?: import("./home").HomeBoardColumns;
  name: string;
  description?: string;
  card_prefix: string;
  source?: Source;
  /** The idle alert (TER-67): `off` or a duration such as `15m`. Absent: the default, 15m. */
  idle_alert?: string;
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
  /** The name of a secret (docs/api.md §5.2), never the secret. */
  credential?: string;
  /**
   * The operator, a sysadmin, or a project file attached the repo, so
   * `credential` can name a global secret with no grant to the project
   * (PLAN §16b.6a). False: a project_admin attached it.
   */
  credential_admin?: boolean;
  path?: string;
  /** A tracked dir whose files each checkout copies into `.git/hooks` (docs/api.md §5.1, "Git hooks"). */
  hooks_dir?: string;
  /** Branch globs that a push must not touch: the `pre-push` hook, and the git proxy for a proxied repo (docs/api.md §5.1). */
  push_deny?: string[];
  /** `proxy` or `direct`; absent: the default rule (docs/api.md §5.1, "The git proxy"). */
  push_via?: 'proxy' | 'direct';
  source?: Source;
  created_at?: string;
}

/** The work tree changes of an agent checkout, as counts (docs/api.md §5.3). */
export interface RepoDirty {
  staged: number;
  unstaged: number;
  untracked: number;
  /** The checkout is large: the counts are a lower bound. */
  partial?: boolean;
}

/** One tree of a repo: `main` (the daemon's mirror) or `agent:<ULID>` (a workspace checkout). */
export interface RepoTree {
  id: string;
  kind: "main" | "agent";
  /** `main`, or `<agent> · <branch>`. */
  label: string;
  branch?: string | null;
  head?: string | null;
  agent?: string;
  agent_id?: string;
  base_ref?: string | null;
  base?: string | null;
  ahead?: number | null;
  behind?: number | null;
  dirty?: RepoDirty | null;
  /** `main` only: the last fetch of the mirror. */
  mirror?: { fetched_at: string | null; error?: string };
  /** The tree cannot be read (a refused repo config, a failed fetch). */
  error?: string;
}

/** `GET /api/repos/{id}/trees`. */
export interface RepoTrees {
  repo: string;
  repo_id: string;
  project: string;
  default_branch?: string | null;
  trees: RepoTree[];
}

export interface TreeEntry {
  name: string;
  path: string;
  kind: "dir" | "file" | "symlink" | "submodule" | "other";
  size?: number;
  /** The target text of a symlink. The daemon does not follow it. */
  target?: string;
}

/** `GET /api/repos/{id}/trees/{tree}/files?path=`. */
export interface TreeListing {
  path: string;
  entries: TreeEntry[];
  /** Over 5000 entries: the rest are not in the answer. */
  truncated: boolean;
}

/** `GET .../file?path=`: the bytes (at most 1 MiB) and what the headers say. */
export interface TreeFile {
  blob: Blob;
  /** `text/plain`: UTF-8 text. Else the bytes are binary. */
  text: boolean;
  /** The size of the whole file. */
  size: number;
  truncated: boolean;
}

export interface FileDiff {
  path: string;
  status: "added" | "deleted" | "modified" | "typechange" | "mode" | "unmerged" | "refused";
  additions: number;
  deletions: number;
  binary: boolean;
  /** Unified diff hunks, from the first `@@` line. */
  patch: string;
  truncated: boolean;
  note?: string;
}

export interface DiffSet {
  files: FileDiff[];
  truncated: boolean;
}

export interface TreeUncommitted {
  staged: DiffSet;
  unstaged: DiffSet;
  untracked: string[];
  untracked_truncated: boolean;
  note?: string;
}

/** `GET .../diff`: the tree against the branch it came from. */
export interface TreeChanges {
  tree: string;
  agent?: string;
  agent_id?: string;
  base?: string | null;
  head?: string | null;
  base_ref?: string | null;
  branch?: string | null;
  committed: DiffSet;
  /** `null` for `main`. */
  uncommitted: TreeUncommitted | null;
  note?: string | null;
}

export interface RepoCommit {
  sha: string;
  parents: string[];
  author_name: string;
  author_email: string;
  date: string;
  subject: string;
}

export interface RepoCommitDetail extends RepoCommit {
  committer_name: string;
  committer_date: string;
  message: string;
  diff: DiffSet;
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
  /** The right to spawn ephemeral agents (docs/api.md §6.10). Absent: no spawn rights. */
  spawn?: SpawnPolicy;
  /** The calendar grant of a lead (docs/api.md §33.3): add events, change the own ones. Absent: read only. */
  calendar_write?: boolean;
}

/** The policy block `spawn` (docs/api.md §6.10): what an agent may spawn. */
export interface SpawnPolicy {
  /** Template names, `global:<name>`, `project:*`, `global:*`, or `*`. */
  templates: string[];
  /** The ephemeral agents of the owner that run at one time. Default 2. */
  max_concurrent?: number;
  /** The spawns in 24 hours. Absent: no limit. */
  max_per_day?: number;
  /** A duration such as `30m` or `2h`. Absent: 2h. At most 24h. */
  max_lifetime?: string;
  /** The ephemeral agents may spawn too, one level (sysadmin or the operator). */
  may_spawn_children?: boolean;
  /** May bind agents to cards or threads. Default false. */
  bound?: boolean;
  /** Bound-agent lifetime cap. Absent: 30d. At most 30d. */
  max_bound_lifetime?: string;
}

/** The outcome of a finished ephemeral agent. */
export type SpawnOutcome = "done" | "failed" | "timeout" | "owner_gone" | "cancelled";

/** The spawn facts on an ephemeral agent (`Agent.spawn`). */
export interface SpawnInfo {
  owner_name: string;
  template: string;
  task: string;
  spawned_at: string;
  /** The end of the lifetime. */
  deadline_at: string;
}

/** One ephemeral agent of an owner (`GET /api/agents/{id}/spawned`, `GET /api/spawns/{id}`). */
export interface Spawn {
  /** The ULID of the ephemeral agent. */
  id: string;
  /** An agent ULID, or `operator`. */
  owner: string;
  owner_name: string;
  project: string;
  name: string;
  template: string;
  harness: Harness;
  task: string;
  /** The first line of the task. */
  summary: string;
  spawned_by: string;
  spawned_at: string;
  deadline_at: string;
  wait_id?: string;
  /** The live status of the agent, or `finished`. */
  status: AgentStatus | "finished";
  finished_at?: string;
  outcome?: SpawnOutcome;
  result?: string;
  /** The current session while it runs; the last one after the finish. */
  session_id?: string;
  duration_secs: number;
}

/** `POST /api/agents/{owner}/spawn`. */
export interface SpawnRequest {
  template: string;
  task: string;
  name?: string;
  /** The project of an ephemeral agent of the operator (`owner` = `operator`). */
  project?: string;
}

/** The answer of a spawn: the spawn, and the agent while it exists. */
export interface Spawned extends Spawn {
  agent?: Agent | null;
}

/** The agent definition, the TOML file as JSON (docs/api.md §6.1). */
export interface AgentDef {
  name: string;
  project: string;
  harness: Harness;
  subscription?: string;
  /** Subscriptions (same provider, the harness can spend each) that a swap may pick. The daemon leaves it out when empty. */
  subscription_pool?: string[];
  model?: string;
  /** The prompt template name under `prompts/`. */
  prompt?: string;
  /** Replaces the harness's default system prompt. Terrarium's protocol prompt is added after it. Absent: the harness default. At most 32 KiB. */
  system_prompt?: string | null;
  /**
   * The persona prompt: `leader`, `worker`, `none`, or a prompt file name.
   * It replaces the harness's default system prompt; `system_prompt` wins
   * over it; `none` keeps the harness default. Absent or null: by role
   * (`leader` for project_admin or sysadmin, else `worker`).
   */
  persona?: string | null;
  roles?: Role[];
  policy?: AgentPolicy;
  restart?: RestartPolicy;
  /** Start a new session, not a resume, when the prompt changed (Codex). The daemon leaves it out when true. */
  new_session_on_prompt_change?: boolean;
  /** The explicit Terrarium-managed skills (`GET /api/skills` names). Absent or empty: none. `"*"`: every skill. The agent also gets the default-on skills. */
  skills?: string[];
  /** The skills that the agent does not get: a default-on skill, or a skill of `skills`. Absent or empty: none. */
  skills_off?: string[];
  /** False: no default-on skill, only `skills`. Absent: true. */
  default_skills?: boolean;
  /**
   * The time that the agent stays `idle` or `waiting` before the daemon stops it:
   * `off` (or `0`) never stops it, else a duration such as `30m` or `1h30m`.
   * Absent: the global default (`GET /api/health` `idle_stop`).
   */
  idle_stop?: string;
  /** Compact the session at the idle stop (each compaction is a model call). Absent: the global default (`GET /api/health` `compact_on_idle`). */
  compact_on_idle?: boolean;
  /** Compact a session older than this at the next rest: `off`, or a duration such as `24h`. Absent: the global default. */
  compact_max_age?: string;
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
      Partial<Omit<AgentDef, "name" | "project" | "subscription" | "model" | "prompt" | "policy" | "idle_stop" | "compact_on_idle" | "compact_max_age">> & {
        /** A template key: a name, `global:<name>`, `<project>/<name>`, or a ULID. */
        template: string;
        /** Merges per key over the template's policy. */
        policy?: Partial<AgentPolicy>;
        subscription?: string | null;
        model?: string | null;
        prompt?: string | null;
        /** `null`: the global default, not the template's value. */
        idle_stop?: string | null;
        /** `null`: the global default, not the template's value. */
        compact_on_idle?: boolean | null;
        /** `null`: the global default, not the template's value. */
        compact_max_age?: string | null;
      });

/** `PATCH /api/agents/{id}`. Name, project, and roles do not change here. `null` clears subscription, model, prompt, or system_prompt; an absent field does not change. A new system_prompt applies at the next start. */
export type AgentPatch = Partial<Pick<AgentDef, "harness" | "policy" | "restart">> & {
  subscription?: string | null;
  /** `[]` clears the pool. */
  subscription_pool?: string[];
  model?: string | null;
  prompt?: string | null;
  system_prompt?: string | null;
  /** `null` goes back to "by role". */
  persona?: string | null;
  /** The skill names; `[]` clears them. An unknown name is a 422. Applies at the next start. */
  skills?: string[];
  /** The skills that the agent does not get; `[]` clears them. Applies at the next start. */
  skills_off?: string[];
  /** False turns off every default-on skill. Applies at the next start. */
  default_skills?: boolean;
  /** `off`, `0`, or a duration. `null` goes back to the global default. */
  idle_stop?: string | null;
  /** `null` goes back to the global default. */
  compact_on_idle?: boolean | null;
  /** `off`, `0`, or a duration. `null` goes back to the global default. */
  compact_max_age?: string | null;
};

/** `subscription_detail` of `GET /api/agents/{id}`: the agent's subscription, when it exists. */
export interface AgentSubscriptionDetail {
  name: string;
  /** The name for people (`Subscription.display_name`). Absent from older daemons. */
  display_name?: string;
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
  /**
   * The subscriptions a swap may pick (docs/api.md §16.4). The current
   * `subscription` is always in it when it is not empty. Absent when empty.
   */
  subscription_pool?: string[];
  model?: string;
  prompt?: string;
  /** Replaces the harness's default system prompt. Absent or null: the harness default. */
  system_prompt?: string | null;
  /** See `AgentDef.persona`. Absent or null: by role. */
  persona?: string | null;
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
  /** The harness version the live agent runs. It stays until the next start, also after an update. Absent when not known. */
  harness_version?: string;
  /** Only in `GET /api/agents/{id}`: the open waits. */
  open_waits?: Wait[];
  /** See `AgentDef.skills`. Absent: none. */
  skills?: string[];
  /** See `AgentDef.skills_off`. */
  skills_off?: string[];
  /** See `AgentDef.default_skills`. Absent: true. */
  default_skills?: boolean;
  /** The skills that the agent gets at its next start: the default-on skills and `skills`, less `skills_off`. Absent: the daemon has no skill library (or is older). */
  effective_skills?: EffectiveSkill[];
  /** True when the live agent runs with other skills than its effective skills now: a restart applies them. */
  skills_restart?: true;
  /** True when the operator paused the agent: it does not run, and nothing wakes it. Turns and mail stay queued. */
  paused?: boolean;
  /** See `AgentDef.idle_stop`. Absent: the global default. */
  idle_stop?: string;
  /** See `AgentDef.compact_on_idle`. Absent: the global default. */
  compact_on_idle?: boolean;
  /** See `AgentDef.compact_max_age`. Absent: the global default. */
  compact_max_age?: string;
  /** False: the daemon cannot compact agents of this harness, and ignores the auto compact settings. */
  compact_supported?: boolean;
  /** The end of the last compaction (RFC 3339). Absent: none. */
  last_compact_at?: string;
  /** The result of the last compaction. Absent: none. */
  last_compact_ok?: boolean;
  /** The reason of the last status change: `idle` for an idle stop, `paused`, `stopped`, crash text, and so on. */
  status_reason?: string;
  /** The turns, mail, and card or nudge pointers that wait for the agent. Absent when 0. */
  queued?: number;
  /** An ephemeral agent (docs/api.md §6.10): it does one task for its owner. Absent when false. */
  ephemeral?: boolean;
  /** The owner of an ephemeral agent: an agent ULID, or `operator`. */
  owner?: string;
  /** The spawn of an ephemeral agent. */
  spawn?: SpawnInfo;
  /** The ephemeral agents of this agent that run now. Absent when 0. */
  spawned_running?: number;
  /** A compaction or a new session that waits (one at most). Absent when none waits. */
  reset_pending?: ResetPending;
}

/** The kind of a reset: a compaction of the session, or a new, clean harness session. */
export type ResetKind = "compact" | "new_session";

/** A reset that waits for the end of the current turn, or for the next wake. */
export interface ResetPending {
  kind: ResetKind;
  /** Who asked: `self`, `operator`, or `project/agent`. */
  by: string;
  /** True when a "then" prompt goes as the next turn after the reset. */
  then_set: boolean;
  /** True when a recap turn goes first (new session only). */
  recap: boolean;
  /** True when the caller asked to wait for the end of the turn. */
  deferred: boolean;
  /** The time of the request (RFC 3339). */
  requested_at: string;
}

/**
 * When a reset runs. `started`: now. `queued`: when the current turn ends
 * (or after the pause, for a paused agent). `next_wake`: the next wake of a
 * stopped agent starts a new session.
 */
export type ResetState = "started" | "queued" | "next_wake";

/** The `reset` field of the answer to `POST .../compact` and `POST .../new-session`. */
export interface ResetOutcome {
  kind: ResetKind;
  state: ResetState;
  /** True when this request replaced a reset that waited. */
  replaced: boolean;
  deferred: boolean;
  then_set: boolean;
}

/** The body of `POST /api/agents/{id}/compact`. */
export interface CompactRequest {
  /** The next turn after the compaction (at most 32 KiB, not blank). */
  then?: string;
  /** True: wait for the end of the current turn. */
  defer?: boolean;
}

/** The body of `POST /api/agents/{id}/new-session`. */
export interface NewSessionRequest extends CompactRequest {
  /** Send a recap turn first. Default: true. */
  recap?: boolean;
}

/** The 202 answer of a reset request: the agent, and how the reset runs. */
export type ResetResult = Agent & { reset: ResetOutcome };

/** How a subscription swap took effect (`POST /api/agents/{id}/subscription`). */
export type SwapApplied = "live" | "restart_scheduled" | "restarted" | "next_start";

/** `POST /api/agents/{id}/subscription`. 409 when the agent is already on it; 422 when the target is not in the pool. */
export interface SwapResult {
  agent: string;
  from?: string;
  to: string;
  applied: SwapApplied;
}

// ----- Waits and watches -----

export type WaitKind = "agent" | "process" | "time" | "card" | "question";
export const WAIT_KINDS: WaitKind[] = ["agent", "process", "time", "card", "question"];
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
/** A `question` wait: it resolves when the question is answered or retracted. */
export interface QuestionWaitSpec {
  question: string;
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
  spec: AgentWaitSpec | ProcessWaitSpec | TimeWaitSpec | CardWaitSpec | QuestionWaitSpec | Record<string, unknown>;
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

// ----- Questions to the operator (docs/api.md §6.8) -----

/**
 * `question`: open → answered | retracted. `notice` (no options): open →
 * acknowledged | retracted. `in_thread`: the operator moved it into an Inbox
 * thread with its agent (GitHub #30); it settles there as an open one.
 */
export type QuestionStatus = "open" | "in_thread" | "answered" | "acknowledged" | "retracted";
/** `critical`: the operator must act now; the dashboard shows a banner on every page and flashes its tab. */
export type QuestionPriority = "normal" | "urgent" | "critical";
/**
 * A `question` asks for a decision. A `notice` only tells the operator something; the operator acknowledges it.
 * A `bundle` holds 2 to 20 related questions (`items`) under one summary (`text`); the operator answers all of them in one step.
 */
export type QuestionKind = "question" | "notice" | "bundle";

/** One question of a bundle, with its answer when the bundle is answered. The agent's words, as for a question. */
export interface BundleItem {
  multi_select?: boolean;
  answer_options?: number[];
  text: string;
  options: string[];
  context?: string | null;
  answer?: string | null;
  /** The 0-based index of the chosen option. */
  answer_option?: number | null;
}

/**
 * A question or a notice of a leader agent to the operator. `text`,
 * `options`, and `context` are the agent's own words: show `text` and
 * `options` as plain text, and `context` (Markdown) only through the
 * DOMPurify sink. For an acknowledged notice, `answer` is the operator's
 * optional reply, and the `answered_*` fields are set.
 */
export interface Question {
  multi_select?: boolean;
  answer_options?: number[];
  id: string;
  kind: QuestionKind;
  /** The asking agent's ULID. */
  agent: string;
  agent_name: string;
  project: string;
  /** The question, the notice, or the summary of a bundle. */
  text: string;
  options: string[];
  /** Kind `bundle` only: the questions, with their answers. */
  items?: BundleItem[];
  context?: string | null;
  card?: string | null;
  priority: QuestionPriority;
  status: QuestionStatus;
  answer?: string | null;
  /** The 0-based index of the chosen option. */
  answer_option?: number | null;
  /** `operator` or the ULID of a sysadmin agent. */
  answered_by?: string | null;
  answered_by_name?: string | null;
  answered_at?: string | null;
  retract_reason?: string | null;
  /** `operator` or `agent:<ulid>`. */
  retracted_by?: string | null;
  thread_id: string;
  /** The Inbox thread that the question was asked in, or moved into (docs/api.md §6.13). Absent on the others. */
  thread?: string;
  created_at: string;
  updated_at: string;
  revision: number;
}

/** The answer to one item of a bundle: a text, an option (label or 0-based index), or both. */
export interface BundleItemAnswer {
  options?: (string | number)[];
  answer?: string;
  option?: string | number;
}

/**
 * `POST /api/questions/{id}/answer`: a text, an option (label or 0-based index), or both. A bundle takes
 * `items` instead: one entry for each item, in order; a missing answer is a 422. `revision`: the revision that
 * the operator saw; another one is a 409 (the agent changed the question meanwhile).
 */
export interface QuestionAnswer {
  options?: (string | number)[];
  answer?: string;
  option?: string | number;
  items?: BundleItemAnswer[];
  revision?: number;
}

/** `data` of a `question` event. */
export interface QuestionEventData {
  question_id: string;
  change: "asked" | "updated" | "answered" | "acknowledged" | "retracted";
  kind: QuestionKind;
  status: QuestionStatus;
  priority: QuestionPriority;
  text: string;
  revision: number;
  agent_id: string;
  card_id?: string;
  /** A change of the card: the card that the question was linked to before. */
  previous_card_id?: string;
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
  /** How many times the daemon resumed this harness session; each resume reuses the row (docs/api.md §6.3). Absent on an older daemon. */
  resumes?: number;
  /** The start of the last resume. */
  resumed_at?: string;
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
/** Who wrote a user turn: the operator (dashboard or CLI), another agent (`agent send`), a mail pointer, the daemon, or keys typed in the terminal. */
export type BlockOrigin = "operator" | "agent" | "agent_mail_pointer" | "system" | "user";

/**
 * One tagged block of a turn that Terrarium typed (PLAN §8.5): its source and
 * the text after the tag line. A mail `body` is quoted with `> ` and can end
 * with an unquoted "… (cut; run `terrarium mail read --thread T`)" line.
 */
export type TurnPart =
  | { source: "user"; body: string }
  /** `sender`: `project/agent`, the agent that typed the turn with `agent send`. */
  | { source: "agent"; sender: string; body: string }
  | { source: "mailbox"; sender: string; subject: string; thread: string; message?: string; body: string }
  /** `kind`: recap, card, lease, nudge, or schedule (`detail` is the schedule name). */
  | { source: "system"; kind: string; detail?: string; body: string }
  /** `thread`: the Inbox thread whose operator message this is (TER-92). */
  | { source: "thread"; thread: string; body: string };

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
  /** The tagged blocks of a user text that Terrarium typed. Absent for an untagged text (the terminal). */
  parts?: TurnPart[];
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
  // The daemon deleted the agent: lists drop it (docs/api.md §9).
  "agent_removed",
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
  "usage_limit",
  // A compaction of the harness: `{trigger?}`, and `{ok, trigger?, message?}` (PLAN §18, "Auto compact").
  "compact_started",
  "compact_finished",
  "mail",
  "card",
  // A file of a card (docs/api.md §8.4): `{change, name, version, size?, sha256?, media_type?, versions?}`.
  "card.file_added",
  "card.file_replaced",
  "card.file_removed",
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
  // The project DB (docs/api.md §27): `data: {change, project, type, id, revision}`.
  "db.type",
  "db.record",
  // A test run (docs/api.md, test runs): `{change, project, id, name, counts, cards, labels}`.
  "test_run",
  // The job queue (docs/api.md §35): a job queued, started, or ended `{change, project, id, card, kind, state, attempt, outcome, cards?}`.
  "job",
  // Monitors (docs/api.md §30): each result, and a change of status `{project, monitor, monitor_id, from, to, value?, unit?, message?}`.
  "monitor.result",
  "monitor.status",
  // Pipelines (docs/api.md §31): a run that is new or changed `{project, pipeline, pipeline_id, run_id, name, from, to, url, branch, commit, environment, summary}`,
  // a change of the pipeline status `{project, pipeline, pipeline_id, from, to, run_id, run_name, url, branch}`, and a poll that failed `{project, pipeline, pipeline_id, error}`.
  "pipeline.run",
  "pipeline.status",
  "pipeline.error",
  // The calendar (docs/api.md §33): an event added, changed (with `changed` and `diff`), or removed
  // `{project, global, event_id, title, kind, start, end, all_day, rrule, assignees}`, and an occurrence that started or
  // ended (also `occurrence`, `late`, `reason`).
  "calendar.event_created",
  "calendar.event_updated",
  "calendar.event_deleted",
  "calendar.event_started",
  "calendar.event_ended",
  // A webhook attempt ended (docs/api.md §19.5): `{trigger, name, delivery, status, http_status, attempt, test, event_seq}`.
  "webhook.delivery",
  "dashboard.updated",
  "target.grant",
  "artifact.created",
  "artifact.linked",
  "artifact.deleted",
  "artifact.pruned",
  // docs/api.md §16.7.
  "usage.sample",
  "quota.sample",
  "quota.alert",
  "quota.blocked",
  "quota.resumed",
  "subscription.offer",
  // docs/api.md §16.7: `{change, harness, from, to}`, the default plan account of a harness.
  "subscription",
  // A subscription was enabled or disabled (`{subscription, enabled}`), or every agent on it was stopped (`{subscription, agents, disabled}`).
  "subscription.enabled",
  "subscription.disabled",
  "subscription.stop_all",
  "self_report",
  // The lifecycle: a pause, an unpause, and a wake (`{cause}`) of a stopped agent.
  "agent.paused",
  "agent.unpaused",
  "agent.woken",
  // The end of a compaction that the daemon ran: `{cause, ok, duration_ms, reason?}`.
  "agent.compacted",
  // A compaction or a new session was asked for: `{kind, by, then_set, deferred, replaced, state}`.
  "agent.reset",
  // Ephemeral agents (docs/api.md §6.10), on the owner: `{spawn_id, name, template, owner_id, owner_name, session_id, task, deadline_at}`
  // and `{spawn_id, name, template, owner_id, owner_name, outcome, session_id, duration_secs}`.
  "agent.spawned",
  "agent.finished",
  // docs/api.md §9, §17.7.
  "lease",
  // Waits: `{wait_id, change, kind, summary, state}`.
  "wait",
  // Watches: `{watch_id, change, on, status?}`.
  "watch",
  // docs/api.md §6.8: `{question_id, change, kind, status, priority, text, revision, agent_id}`.
  "question",
  // The operator's inbox: `{item, kind, change, project?}` on each change of an item.
  "inbox",
  // docs/api.md §6.13: `{change, id, scope, status}`, an Inbox thread.
  "thread",
  // docs/api.md §4: a harness update in the container.
  "harness",
  // docs/api.md §19: `{change, schedule, run?, status?}`.
  "schedule",
  // docs/api.md §20: `{change, source, name, key, kind, enabled}`.
  "event_source",
  // docs/api.md §21: `{change, template, name, key, revision}`.
  "template",
  // docs/api.md §28: `{change, name, path}`.
  "system_prompt",
] as const;
export type EventKind = (typeof EVENT_KINDS)[number];

/**
 * The session link of a log row: an event, a message, or an audit row
 * (docs/api.md §4, §7, §9). The daemon sends it only when the reader may
 * read the session and the session can still open.
 */
export interface LogSessionLink {
  /** The agent session that was active when the row was written. The session page is `/sessions/<id>`. */
  session_id?: string;
  /** `<agent> · <start time>`, for example `dev · 2026-10-26 09:12 UTC`. */
  session_label?: string;
}

/** One row of the event log (docs/api.md §9). */
export interface AgentEvent extends LogSessionLink {
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
  /** A `seq` or an event ID. Only later events come. With `order: "desc"`, the lower bound of every page. */
  since?: number | string;
  /** `desc`: newest first, and the cursor goes on to older events. JSON pages of `/api/events` and `/api/agents/{id}/events` only. */
  order?: "asc" | "desc";
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

export interface Message extends LogSessionLink {
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
  /** The operator released a message that the loop guard held: the recipient got its pointer then. */
  released_at?: string;
  released_by?: string;
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
  /** The messages that touch the project. */
  project?: string;
  /** RFC 3339 times. */
  since?: string;
  until?: string;
  /** Free text over the subject, the body, and the names. */
  q?: string;
  limit?: number;
  cursor?: string;
}

export interface ProjectInboxQuery {
  unread?: boolean;
  state?: MessageState;
  thread?: string;
  limit?: number;
  cursor?: string;
}

// ----- The operator's inbox: what waits for the operator -----

export type InboxKind = "question" | "notice" | "mail" | "system" | "watch" | "thread";
export type InboxStatus = "open" | "done" | "all";

/** A thread that the loop guard paused, with the messages that it held. */
export interface PausedThread {
  kind: "paused_thread";
  thread_id: string;
  subject: string;
  /** The loop guard pauses the thread now. */
  paused: boolean;
  /** Agent-to-agent messages since the last reset. */
  agent_run: number;
  /** `TERRARIUM_MAIL_LOOP_LIMIT`. */
  limit: number;
  /** Display names. */
  participants: string[];
  /** The held messages that are not released. */
  held: Message[];
}

/**
 * A watched thread (docs/api.md §6.9, "Ask and forward"): a question that the
 * operator asked an agent, or a question of the Inbox that the operator
 * forwarded to one. It waits for the agent while `waiting` is true.
 */
export interface InboxWatch {
  thread_id: string;
  /** The agent's ULID. */
  agent: string;
  /** `project/name`. */
  agent_name: string;
  /** The forwarded question, bundle, or notice. Absent on an ask. */
  question_id?: string;
  /** The ask: the first message of the thread. */
  message_id: string;
  subject: string;
  created_at: string;
  cancelled_at?: string;
  /** The newest message of the thread is the operator's. */
  waiting: boolean;
  /** The messages of the agent on the thread. */
  replies: number;
}

/** `POST /api/inbox/asks`: `to` is the agent; `text` the question, or a note on a forward of `question`. */
export interface InboxAskBody {
  to: string;
  text?: string;
  question?: string;
}

/** The answer of `POST /api/inbox/asks`: the mail and the new watch. */
export interface InboxAsk {
  message: Message;
  watch: InboxWatch;
}

/**
 * One thing that waits for the operator (`GET /api/inbox`). `id` is
 * `question:<id>`, `bundle:<id>`, `notice:<id>`, `mail:<message id>`, `thread:<thread id>`, `watch:<thread id>`, or `inbox-thread:<id>`.
 * A `watch` item waits for an agent, not for the operator.
 * A bundle is an item of the kind `question`.
 * A system item shows only while it is open.
 */
export interface InboxItem {
  id: string;
  kind: InboxKind;
  open: boolean;
  /** Open, with the priority `urgent` or `critical`. */
  urgent: boolean;
  /** Open, with the priority `critical`. Absent on an older daemon. */
  critical?: boolean;
  project: string | null;
  /** When it came in: the sort key. */
  at: string;
  /** Kind `question` or `notice`. */
  question?: Question;
  /** Kind `mail`. */
  message?: Message;
  /** Kind `system`. */
  system?: PausedThread;
  /** Kind `watch`: the watch. Kind `mail` on a watched thread: the watch that the mail answers. */
  watch?: InboxWatch;
  /** Kind `thread`: the Inbox thread as `GET /api/threads` lists it (docs/api.md §6.13). */
  thread?: Thread;
}

export interface InboxQuery {
  /** JSON tuple [open, critical, urgent, at, id] of the previous page's last item. */
  cursor?: string;
  limit?: number;
  status?: InboxStatus;
  kind?: InboxKind;
  project?: string;
}

/** `GET /api/projects/{name}/counts` (TER-154): the counts of the project tabs. */
export interface ProjectCounts {
  /** The cards in the `blocked` role columns, archived ones excluded. */
  blocked_cards: number;
  /** The open questions, bundles, and tool run approvals of the project, also those in a thread. `null` for a reader who may not list them. */
  questions: { open: number; urgent: number; critical: number } | null;
}

/** `GET /api/inbox/counts`: the open items. `urgent` counts the open urgent and critical items; `critical` only the critical ones. */
export interface InboxCounts {
  open: number;
  urgent: number;
  /** Absent on an older daemon. */
  critical?: number;
  question: number;
  notice: number;
  mail: number;
  system: number;
  /** The open watches. Not in `open`: a watch waits for an agent. Absent on an older daemon. */
  watch?: number;
  /** The open Inbox threads: the Threads badge. Not in `open`, `urgent`, or `critical` (TER-466). Absent on an older daemon. */
  thread?: number;
  /** The open threads with an urgent or critical open question. Absent on an older daemon. */
  thread_urgent?: number;
  /** The open threads with a critical open question. Absent on an older daemon. */
  thread_critical?: number;
}

/** `POST /api/inbox/mail/{id}/ack`: the acknowledged message, and the replies (one for each participant). */
export interface MailAck {
  message: Message;
  replies: Message[];
}

/** `POST /api/mail/threads/{id}/resume`: the released message IDs, and the note messages. */
export interface ThreadResume {
  thread_id: string;
  released: string[];
  note: Message[];
}

/** `data` of an `inbox` event. */
export interface InboxEventData {
  item: string;
  kind: InboxKind;
  change: "opened" | "updated" | "closed";
  project?: string;
}

// ----- Inbox threads (docs/api.md §6.13) -----

/** Who must act on a thread. `tracking` (TER-145): the agent tracks handed-off work, parked. */
export type ThreadStatus = "waiting_operator" | "waiting_agent" | "tracking" | "closed";

/** One message of a thread. `text` is Markdown: untrusted data. */
export interface ThreadMessage {
  id: string;
  /** `operator`, or an agent ULID. */
  author: string;
  author_name: string;
  /** `response`: the operator's overall response. */
  kind: "message" | "response";
  text: string;
  at: string;
}

/** The operator's overall response: the approval record. */
export interface ThreadResponse {
  text: string;
  by: string;
  at: string;
}

export type ThreadLinkKind = "card" | "mail" | "wiki" | "memory" | "url" | "other";

export interface ThreadLink {
  kind: ThreadLinkKind;
  ref: string;
}

/** Set at the close. */
export interface ThreadHandoff {
  summary: string;
  links: ThreadLink[];
  by: string;
  at: string;
}

/**
 * An Inbox thread: a conversation between the operator and one agent on one
 * topic. `GET /api/threads/{id}` has `messages` and `questions`; a list has
 * `message_count`, `open_questions`, and `last_at` instead.
 */
export interface Thread {
  id: string;
  topic: string;
  /** `global`, or a project name. */
  scope: string;
  template: string | null;
  /** `operator`, or an agent ULID. */
  opened_by: string;
  opened_by_name: string;
  agent: string | null;
  agent_name: string | null;
  report_to: string | null;
  report_to_name: string | null;
  /** The outcome when the attached agent ended while this thread was open. */
  agent_end_outcome?: string | null;
  status: ThreadStatus;
  /** When the agent moved the thread into tracking (TER-145); set while it tracks. An older daemon sends none. */
  tracking_at?: string | null;
  /** The question or notice that the operator moved into this thread (GitHub #30). */
  from_question?: string;
  /** The linked cards whose project the reader may read, the oldest link first (TER-146); also in a list (TER-147). An older daemon sends none. */
  cards?: ThreadCard[];
  messages?: ThreadMessage[];
  questions?: Question[];
  response: ThreadResponse | null;
  handoff: ThreadHandoff | null;
  message_count?: number;
  open_questions?: number;
  last_at?: string;
  created_at: string;
  updated_at: string;
  closed_at: string | null;
  closed_by: string | null;
  /** The close that the attached agent proposes (TER-95): the handoff that the operator confirms. `null`: none. Absent on a daemon before 0.3.0. */
  close_proposal?: ThreadHandoff | null;
  /** Why a closed thread closed. Absent on a daemon before 0.3.0. */
  close_reason?: ThreadCloseReason | null;
}

/** A card that a thread tracks (TER-146, docs/api.md §6.13 "Linked cards"): only the reference; the dashboard reads the card from its board. */
export interface ThreadCard {
  project: string;
  card: string;
  linked_by: string;
  linked_by_name?: string;
  linked_at: string;
}

/** `confirmed`: the operator confirmed the agent's proposal; `operator`: the operator closed it; `stale`: untouched; `failed_start`: its agent did not start. */
export type ThreadCloseReason = "confirmed" | "operator" | "stale" | "failed_start" | "agent_ended";

/** `POST /api/threads`. `agent`: a ULID or `project/name`, until TER-92 spawns it from the template. */
export interface NewThread {
  /** Opens a request with global:request, reporting to this project lead. */
  report_to?: string;
  topic: string;
  scope?: string;
  template?: string;
  agent?: string;
  text?: string;
}

export interface ThreadQuery {
  /** Default `open`: not closed. */
  status?: ThreadStatus | "open" | "all";
  scope?: string;
}

/** `POST /api/threads/{id}/close`: the handoff, or `confirm` (TER-95): close with the agent's proposed handoff; no `summary` or `links` with it. */
export interface ThreadClose {
  summary?: string;
  links?: ThreadLink[];
  confirm?: boolean;
}

/** `data` of a `thread` event. */
export interface ThreadEventData {
  change: "opened" | "message" | "response" | "question" | "tracking" | "close_proposed" | "closed";
  id: string;
  scope: string;
  status: ThreadStatus;
}

// ----- Board (§13, docs/api.md §8) -----

/** The role of a column (docs/api.md §8.1): `working` is where `card claim` moves a card; a move to `blocked` needs a comment. */
export type ColumnRole = "working" | "blocked" | "validating" | "review" | "merge_queue" | "accepted";
export const COLUMN_ROLES: ColumnRole[] = ["working", "blocked", "validating", "review", "merge_queue", "accepted"];

export interface Column {
  name: string;
  /** The display label: the stored title, or the name with spaces for underscores. An older daemon sends none. */
  title?: string;
  position: number;
  wip_limit?: number;
  /** `working`, `blocked`, or none. */
  role?: ColumnRole;
  /** Cards in the column, archived ones excluded. */
  count?: number;
}

/**
 * A column in `PATCH /api/projects/{name}/board`: the list is the whole board, in order.
 * `title` and `role` absent keep the current value; `null` clears it. `wip_limit` absent: no limit.
 */
export interface ColumnEdit {
  name: string;
  title?: string | null;
  wip_limit?: number;
  role?: ColumnRole | null;
  rename_from?: string;
}

export interface CardComment {
  id?: string;
  author: string;
  author_name?: string;
  body: string;
  created_at: string;
  /** docs/api.md §8: the session of the agent that wrote the comment. `null` or absent for the operator and an older daemon. */
  session?: SessionRef | null;
}

/** docs/api.md §6.6: a session with enough of its agent to render a link. It stays after `agent rm` (`removed: true`). */
export interface SessionRef {
  id: string;
  agent_id: string;
  /** The agent name, without the project. */
  agent: string;
  project: string;
  harness: Harness;
  started_at: string;
  ended_at?: string;
  /** The agent is deleted; the transcript comes from the session archive. */
  removed: boolean;
}

/** `GET /api/sessions/{id}` (docs/api.md §6.6). */
export interface SessionDetail extends SessionRef {
  harness_session_id?: string;
  /** The agent's current session, and the agent runs. */
  active: boolean;
}

export type CardLinkKind = "thread" | "branch" | "card" | "url";

export interface CardLink {
  kind: CardLinkKind;
  ref: string;
  created_by?: string;
  created_at?: string;
}

/** A file of a card (docs/api.md §8.4): the latest version of one name. */
export interface CardFile {
  card: string;
  name: string;
  version: number;
  size: number;
  sha256: string;
  /** For example `text/markdown; charset=utf-8`, `image/png`. */
  media_type: string;
  /** `operator` or an agent ULID: who stored this version. */
  created_by: string;
  created_by_name: string;
  created_at: string;
  /** The stored versions of the name. */
  versions: number;
}

/** One version of a card file (`GET /api/cards/{id}/files/{name}/versions`). */
export interface CardFileVersion {
  name: string;
  version: number;
  size: number;
  sha256: string;
  media_type: string;
  created_by: string;
  created_by_name: string;
  created_at: string;
}

/** The answer of an upload: the file, and what the upload did. */
export interface CardFileUploaded extends CardFile {
  outcome: "added" | "replaced" | "unchanged";
}

/** Reader-filtered wait state supplied by the daemon, never inferred from the board. */
export interface CardWait {
  dependencies: { card: string | null; target?: string; met: boolean; hidden: boolean }[];
  mode: "all" | "any";
  action: { move_to?: string | null; comment?: string | null; wake: boolean };
  met_at?: string | null;
  generation?: string;
  wake_recipient?: string | null;
}

export interface Card {
  wait?: CardWait | null;
  kind?: "card" | "group";
  group?: BoardGroup;
  id: string;
  project: string;
  column: string;
  column_role?: ColumnRole | null;
  position: number;
  title: string;
  body: string;
  /** Stored address: an agent ULID or `operator`. */
  assignee?: string;
  /** Readable: `project/agent` or `operator`. */
  assignee_name?: string;
  /** The card agent (TER-75): the ULID of the open ephemeral agent bound to the card. It stays while the card waits. */
  agent?: string;
  agent_name?: string;
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
  /** docs/api.md §8: when the work is expected to be done (RFC 3339). It stays on a done or archived card. */
  eta_at?: string | null;
  /** When the ETA was set: the start of the countdown's progress bar. */
  eta_set_at?: string | null;
  /** The address that set the ETA: an agent ULID or `operator`. */
  eta_set_by?: string | null;
  eta_set_by_name?: string | null;
  eta_calibrated?: boolean | null;
  /** When the card last moved to `done`; a reopen clears it. */
  done_at?: string | null;
  /** The latest test run linked to the card (docs/api.md, test runs). Absent when no run is linked. */
  tests?: CardTests;
  /** The idle mark (docs/api.md §6.2 "Idle alert", TER-67): the assignee rests with no wait since this time, past the project's idle alert, and the leads were told. */
  idle_since?: string | null;
  /** Active trouble signals, also shown on the card's usage view. */
  trouble?: string[];
  /** The open (queued or running) job of the card (docs/api.md §8, TER-158). Absent when the card has no open job. */
  job?: CardJob;
  /** The stored status of the assignee agent. Absent for the operator, no assignee, or a removed agent. */
  assignee_status?: string;
}

export interface BoardGroup {
  steps: { name: string; state: "pending" | "running" | "done" | "failed"; text: string; eta_at: string | null; started_at?: string | null; duration_secs?: number | null; estimated_secs?: number | null }[];
  id: string;
  key: string;
  title: string;
  column: string;
  status: string;
  done: number;
  total: number;
  members: string[];
  eta_at: string | null;
  closed_at: string | null;
}

/** The open job of a card: the tile reads its state chip from it, not from labels. */
export interface CardJob {
  id: string;
  kind: "validate" | "merge";
  state: "queued" | "running";
  /** A queued job: its place among the queued jobs of the project (1 starts next). */
  place?: number;
  /** A queued job: the count of queued jobs in the project. */
  queued?: number;
  /** The planned duration in seconds (TER-155 D2): the card's ETA is the job start plus it. */
  planned_secs?: number;
}

/** The first-attempt numbers of the jobs of one kind, or of all (docs/api.md §35.4, TER-135). */
export interface FirstAttempts {
  /** Ended first attempts that ran the recipe: `passed + failed`. */
  ended: number;
  passed: number;
  /** Failed with `red` (or a timeout). A `conflict` and an `error` never ran the tests, so they count outside this. */
  failed: number;
  /** `passed / ended`, from 0 to 1. `null` with no ended first attempt. */
  pass_rate: number | null;
}

/** A test target that failed, and how often (TER-135). */
export interface TargetFails {
  /** As cargo names it: `-p terrariumd --test jobs`. */
  target: string;
  /** The failed jobs whose tail names the target, all attempts. */
  failed: number;
  /** Of them, the first attempts. */
  first_attempt: number;
}

/** A recipe step that failed, and how often (TER-135). */
export interface StepFails {
  step: string;
  failed: number;
}

/** The pass rate of a project's job queue over a window: `GET /api/projects/{name}/jobs/stats` (docs/api.md §35.4, TER-135). */
export interface JobStats {
  project: string;
  since: string;
  /** All jobs in the window, in all states. */
  jobs: number;
  first_attempt: FirstAttempts;
  validate: FirstAttempts;
  merge: FirstAttempts;
  /** Ended reruns (attempt 2 or more): `passed` is a failure that the rerun did not repeat. */
  reruns: FirstAttempts;
  /** First attempts that ended in a merge conflict. */
  conflicts: number;
  /** First attempts and reruns that could not run (outcome `error`). */
  errors: number;
  cancelled: number;
  /** Queued or running. */
  open: number;
  /** The halves of a split batch. */
  splits: number;
  /** The failed recipe steps (`test`, `lint`, ...; `timeout` when the job was killed), most first. */
  failed_steps: StepFails[];
  /** The test targets that failed, most first. */
  failed_targets: TargetFails[];
  /** Failed `test` steps whose tail names no target (the 4000-byte tail clip cut the header off). */
  untargeted_test_fails: number;
}

/** A card of `GET /api/cards` (docs/api.md §8, TER-43): a card in the working or research column of a board, with its column kind and the status of its assignee agent. */
export interface ActiveCard extends Card {
  /** `working` (the column with that role), `research`, or another column that the query named. */
  column_kind: string;
  /** A card in the blocked column (TER-51): when it moved there. */
  blocked_since?: string;
  /** A card in the blocked column: the comment of that move, or the newest comment without one. */
  blocked_reason?: string;
}

/** List-only card. Fetch cards.get(id) before displaying or editing details. */
export interface CardWaitSummary {
  mode: "all" | "any";
  met: boolean;
  deps_met: number;
  deps_total: number;
}

export type CardSummary = Omit<Card, "body" | "comments" | "links" | "wait"> & {
  wait?: CardWaitSummary | null;
  comment_count?: number;
  last_comment_at?: string | null;
};

export interface Board {
  project: string;
  card_prefix?: string;
  columns: Column[];
  cards: CardSummary[];
  next_cursor?: string | null;
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
  /** An RFC 3339 time or a duration from now (`2h30m`): in the future, at most 30 days away. */
  eta_at?: string;
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
  /** An RFC 3339 time or a duration from now (`2h30m`); `null` clears the ETA. */
  eta_at?: string | null;
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
  /** A link stored before TER-58 can be `version`. */
  kind: "page" | "card" | "memory" | "artifact" | "version" | string;
  /** `project/path`, a card ID, `project/MEM-n`, `project/ART-n`, or (before TER-58) `project/<version>`. */
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

// ----- The project DB (PLAN §15.4, docs/api.md §27) -----

export type DbKind = "string" | "text" | "number" | "integer" | "bool" | "date" | "datetime" | "enum" | "list" | "ref" | "json";
export type DbItemKind = "string" | "integer" | "number";

/** One field of a record type. */
export interface DbField {
  name: string;
  kind: DbKind;
  required: boolean;
  /** The value of a new record that gives none. */
  default?: unknown;
  description?: string;
  /** `enum`: the values, in sort order. */
  values?: string[];
  /** `list`: the kind of the items. */
  of?: DbItemKind;
  /** `ref`: the name of the target type. */
  ref?: string;
  /** Absent when false. */
  unique?: boolean;
}

export interface DbType {
  id: string;
  project: string;
  name: string;
  description: string;
  fields: DbField[];
  revision: number;
  /** The number of records of the type. */
  records: number;
  created_by: string;
  created_by_name: string;
  created_at: string;
  updated_at: string;
}

export interface NewDbType {
  name: string;
  description?: string;
  fields: DbField[];
}

/** `PATCH .../db/types/{name}`: `fields` is the whole new list, after the renames. */
export interface DbTypePatch {
  revision: number;
  description?: string;
  fields?: DbField[];
  rename?: { from: string; to: string }[];
  /** Needed when a field goes away. */
  confirm?: boolean;
}

export interface DbRecord {
  id: string;
  project: string;
  type: string;
  data: Record<string, unknown>;
  revision: number;
  created_by: string;
  created_by_name: string;
  updated_by: string;
  updated_by_name: string;
  created_at: string;
  updated_at: string;
}

/** A column of a query answer: a field, or a record property (`meta`: id, created, updated, created_by, updated_by, revision). */
export type DbColumn = DbField & { meta?: boolean };

export type DbView = "table" | "list" | "count" | "group";

/** `GET /api/projects/{p}/db/query`. */
export interface DbQueryResult {
  type: string;
  query: string;
  view: DbView;
  fields: DbColumn[];
  /** `data` has only the columns. Empty for `count:true` and `group:`. */
  records: DbRecord[];
  /** The number of records that match (all pages). */
  count: number;
  next_cursor?: string;
  group?: string;
  groups?: { value: unknown; count: number }[];
  /** A label for each record ID in a ref column: the first string field of the record. */
  labels: Record<string, string>;
}

// ----- Artifacts (§15.2, docs/api.md §12) -----

/** `label`: a label such as `version:1.4.0` (TER-58). Version objects are removed. */
export type ArtifactLinkKind = "page" | "card" | "label";

/** One pinned conversation (`GET /api/pins`, docs/api.md §6.11). Oldest pin first. */
export interface Pin {
  /** The agent ULID. */
  agent: string;
  project: string;
  name: string;
  pinned_at: string;
}

/** One project folder of the sidebar (`GET /api/project-folders`, docs/api.md §6.12). `parent_id` null is the root. */
export interface ProjectFolder {
  id: string;
  name: string;
  parent_id: string | null;
  position: number;
  created_at: string;
  updated_at: string;
}

/** The folder of one project (docs/api.md §6.12). A project with no placement sits at the root, after the placed ones. */
export interface ProjectPlacement {
  project: string;
  folder_id: string | null;
  position: number;
}

/** `GET /api/project-folders`: every folder and every placed project. */
export interface ProjectFolderTree {
  folders: ProjectFolder[];
  placements: ProjectPlacement[];
}

/** `POST /api/agents/{id}/attachments` (docs/api.md §6.5): an image in the agent's workspace. */
export interface Attachment {
  /** The path of the file as the agent sees it (in the sandbox). Never the host path. */
  path: string;
  /** The file name: `<YYYYMMDD-HHMMSS>-<id>.<ext>`. */
  name: string;
  bytes: number;
  content_type: "image/png" | "image/jpeg" | "image/gif" | "image/webp";
}

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
  /** A page key, a card ID of the same project, or a label (`version:1.4.0`). */
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

// ----- Subscriptions and usage (docs/api.md §16) -----

/** `unknown`: not checked yet. */
/** `api_key`: a stored Z.ai key; no harness login to check. */
export type LoginStatus = "logged_in" | "expiring" | "expired" | "missing" | "unknown" | "api_key";
/** `none`: no probe, or not run yet. */
export type ProbeStatus = "ok" | "failed" | "none";

/** The newest sample of one window (docs/api.md §16.1). */
export interface QuotaWindow {
  /**
   * `5h`, `7d`, `7d_<model>` (a weekly limit of one model), `extra`,
   * `<n>h`, `<n>d`, `monthly`, ... Only the windows that the last good
   * probe reported: an Anthropic Ultra plan has no `5h`.
   */
  window: string;
  used_pct: number;
  resets_at?: string;
  /** When the probe read it. */
  at: string;
  /** The money left, when the window is a credit balance (`openrouter-credits`). */
  balance?: { total: number; used: number; remaining: number; currency: string };
}

/** The plan account of a subscription (docs/api.md §16.4): one harness login of its own. */
export interface PlanAccount {
  harness: LoginKind;
  has_login: boolean;
  /** The login name (an email or a user name), when known. */
  login?: string;
  /** `migration`: moved from the shared harness home. `login`: made by the login wizard. */
  source: "migration" | "login";
  created_at: string;
  updated_at: string;
}

/** An agent whose current subscription this is. */
export interface SubscriptionAgentRef {
  id: string;
  name: string;
  project: string;
}

/** docs/api.md §16.1. */
export interface Subscription {
  fallback_spend_usd?: number;
  cloud?: CloudSettings;
  month_spend_usd?: number;
  session_period?: { since: string; until: string };
  sessions?: Array<{ id: string; agent: string; project: string; harness: string; started_at: string; model?: string; card?: string; input_tokens?: number; output_tokens?: number }>;
  name: string;
  /**
   * The name for people: the operator's `label`, else the vendor's product
   * and plan name (`Claude Ultra`, `ChatGPT Pro`, `GitHub Copilot`). `name`
   * stays the id. Absent from older daemons: use `subscriptionTitle`.
   */
  display_name?: string;
  /** The display name that the operator set (`PUT /api/subscriptions/{name}/label`). */
  label?: string;
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
  /** Agents whose turns wait after a usage-limit error on this subscription. */
  held_agents: number;
  /**
   * The model each harness gets on this subscription when the agent sets no
   * `model`. Only harnesses where the daemon picks it (pi, Claude Code on
   * Z.ai). Left out when there is none, and by older daemons.
   */
  default_models?: Partial<Record<Harness, string>>;
  /** The plan account. Absent when the subscription has none (an API-key subscription). */
  account?: PlanAccount;
  /** The agents whose current subscription this is. */
  agents: SubscriptionAgentRef[];
  /**
   * The account is the default account of its harness (PLAN §16.3): a
   * Claude Code, Codex, or Copilot agent with no subscription runs on it.
   * Absent from older daemons.
   */
  default?: boolean;
  /** The server of a `selfhosted` subscription. Absent for every other provider. */
  selfhosted?: SelfHosted;
  /** False: no agent on it starts or wakes, and no new agent takes it. Agents that run go on. Absent from older daemons (enabled). */
  enabled?: boolean;
  /** The provider has no quota at all (a self-hosted server): show the usage counts only. */
  no_quota?: boolean;
  /** The streams of a self-hosted subscription with `max_concurrent`. */
  streams?: StreamUsage;
  /** The public status page of the provider, or the `status_url` of a self-hosted server. */
  status_url?: string;
}

/** The API style of a self-hosted server. */
export type SelfHostedApi = "openai" | "anthropic";

/** The server of a `selfhosted` subscription (docs/api.md §16). No secret: the key is in the key store. */
export interface SelfHosted {
  /** LiteLLM monthly fallback budget, counted from session costs. Mail only. */
  budget_usd?: number;
  api: SelfHostedApi;
  /** `http` or `https`. The OpenAI root with `/v1`, or the Anthropic root without it. */
  base_url: string;
  models?: string[];
  default_model?: string;
  context_window?: number;
  max_output_tokens?: number;
  /** How many agents may run a turn at the same time. Absent: no limit. */
  max_concurrent?: number;
  status_url?: string;
}

/** The streams of a limited self-hosted subscription. */
export interface StreamUsage {
  max: number;
  in_use: number;
  waiting: number;
}

/** `PATCH /api/subscriptions/{name}`. In `selfhosted`, `null` clears an optional field. */
export interface SubscriptionPatch {
  alert_at?: number;
  harnesses?: Harness[];
  selfhosted?: Partial<{ [K in keyof SelfHosted]: SelfHosted[K] | null }>;
}

/** `POST /api/subscriptions/{name}/stop-all`. */
export interface StopAllResult {
  subscription: Subscription;
  stopped: { id: string; name: string; project: string; error?: string }[];
}

/** `POST /api/subscriptions/fetch-models`. */
export interface FetchModelsInput {
  api?: SelfHostedApi;
  base_url?: string;
  /** Sent to the server once; never stored. */
  key?: string;
  /** Use the server and the stored key of this subscription. */
  subscription?: string;
  refresh?: boolean;
}

export interface FetchModelsResult {
  api: SelfHostedApi;
  base_url: string;
  models: ModelInfo[];
  cached: boolean;
  fetched_at: string;
}

/** Where `GET /api/subscriptions/{name}/models` found the list: the provider's `/models`, the harness login's list, pi's catalogue, the default table, or the stored list of a self-hosted subscription. */
export type ModelSource = "provider" | "plan" | "pi" | "fallback" | "subscription";

export interface ModelInfo {
  id: string;
  name?: string;
  /** The context window in tokens. */
  context?: number;
}

/** `GET /api/subscriptions/{name}/models` (docs/api.md §16.4). */
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
  /** For provider `selfhosted`. */
  selfhosted?: SelfHosted;
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
export interface CloudSettings {
  budget_usd?: number | null;
  region?: string;
  project_id?: string;
  resource?: string;
  base_url?: string;
  models?: Record<string, string>;
}

export interface ApiKeyInput {
  provider: string;
  key: string;
  key_id?: string;
  cloud?: CloudSettings;
  test_connection?: boolean;
  /**
   * `zai`: `coding` (GLM Coding Plan, the default) or `api` (pay as you go).
   * `moonshot` (pi only): `coding` (Kimi Code, the default), `coding-cn`, `api` (Open Platform), or `api-cn`.
   * No other provider takes a plan.
   */
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

/** `GET /api/cards/{id}/usage` (docs/api.md §16.4): the samples attributed to one card. */
export interface CardUsage {
  working_seconds?: number;
  signals?: string[];
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
  subscription?: Pick<Subscription, "name" | "display_name" | "provider" | "plan" | "probe_status" | "last_success_at" | "local_estimate_only" | "windows">;
  today: { since: string; mine: UsageTotals; subscription?: UsageTotals; share_pct?: number };
  holds: QuotaHold[];
}

// ----- Targets and hosts (PLAN §17, §19, docs/host-protocol.md) -----
// Coded to the daemon's `api/hosts.rs` and `api/targets.rs`, which are new
// and not yet in docs/api.md. The mock serves the same shapes.

/** `exedev`: a VM on exe.dev per holder, reached over ssh (TER-294). The create dialog does not offer it. */
export type TargetKind = "host" | "ssh" | "docker" | "vmlab" | "vnc" | "rdp" | "desktop" | "exedev";
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
  /** Only with a `credential` (docs/api.md §17.2): the secret that the daemon uses. Never a value. */
  credential_info?: CredentialInfo;
  /** `host` and `docker`: the resource limits as the operator wrote them (docs/api.md §17.2, "Resource limits"). */
  limits?: TargetLimits;
  /** Only with `limits`: the short text, who shares the budget, and why the host agent cannot apply them. */
  limits_info?: LimitsInfo;
}

/** docs/api.md §17.2, "Resource limits": the values as written. The daemon checks them (`422 invalid`). */
export interface TargetLimits {
  /** CPU time as cores: `2`, or `0.5` for half of one core. */
  cpu?: number | string;
  /** The memory cap: bytes, or a size such as `"4G"` (powers of 1024). */
  memory?: number | string;
  /** Swap on top of `memory`. Default: no swap. */
  swap?: number | string;
  /** The most processes and threads. */
  pids?: number | string;
  /** The IO weight, 1 to 10000. */
  io_weight?: number | string;
}

/** docs/api.md §17.2: the `limits_info` of a target view. */
export interface LimitsInfo {
  /** For example `2 CPU · 4G · 512 pids`. */
  summary: string;
  /** `holder`: each agent on a host target gets its own budget. `container`: the agents of a docker target share it. */
  per: "holder" | "container";
  /** Why the online host agent cannot apply the limits; a session (or an `up`) is then refused. */
  problem?: string;
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
  limits?: TargetLimits;
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
export interface ResourceAmounts {
  cpu_millis: number;
  memory_bytes: number;
  io?: Record<string, { read_bps: number; write_bps: number; read_iops: number; write_iops: number }>;
}
export interface HostCapacityBudget {
  host: string;
  percentage: number;
  physical: { cpu_ids: number[]; memory_bytes: number };
  reservable: ResourceAmounts;
  used: ResourceAmounts & { exclusive_cores?: number[] };
}
export interface Reservation {
  id: string;
  host: string;
  holder: { kind: "agent" | "target"; id: string };
  amounts: ResourceAmounts;
  state: "queued" | "starting" | "active" | "ending" | "expired" | "retracted" | "cancelled" | "timed_out";
  exclusive: boolean;
  reason: string;
  start: number | null;
  until: number | null;
  queue_until: number | null;
}
export interface HostCapabilities {
  reservations?: boolean;
  capacity?: { cpu_ids: number[]; memory_bytes: number };
  docker?: ToolCapability;
  vmlab?: ToolCapability;
  shell?: string;
  /** Target displays (docs/api.md §17.10). Absent from an older host agent: off. */
  display?: DisplayCapability;
  /** The host agent sends performance samples (docs/api.md §17.1). Absent from an older host agent. */
  metrics?: boolean;
  /** What the host agent can do for target resource limits (docs/host-protocol.md, "Resource limits"). Absent from an older host agent: none. */
  limits?: LimitsCapability;
}

/** `terrarium_core::limits::LimitsCapability`. */
export interface LimitsCapability {
  /** `systemd-run --user` works: a host target can have limits. */
  systemd: boolean;
  /** The cgroup controllers of the systemd user manager. */
  controllers?: string[];
  /** Why `systemd` is false. */
  reason?: string;
  /** `docker run` takes the limits. */
  docker: boolean;
}

/** `terrarium_host::protocol::HostMetrics` with the daemon's `at` (docs/host-protocol.md, "`metrics`"). Rates are bytes per second. A part the host agent cannot read is absent. */
export interface HostMetricsSample {
  /** When the daemon got the sample. */
  at: string;
  /** The seconds that the rates cover. */
  interval_secs: number;
  cpu?: { busy_pct: number; cores: number; load1: number };
  memory?: { total_bytes: number; used_bytes: number; used_pct: number; swap_total_bytes: number; swap_used_bytes: number };
  net?: { rx_bytes_per_sec: number; tx_bytes_per_sec: number };
  disk_io?: { read_bytes_per_sec: number; write_bytes_per_sec: number };
  filesystems: HostFilesystem[];
}

export interface HostFilesystem {
  mount: string;
  fstype: string;
  device: string;
  total_bytes: number;
  used_bytes: number;
  avail_bytes: number;
  /** `used / (used + avail)`, as `df` shows it. */
  used_pct: number;
}

/** `GET /api/hosts/{name}/metrics?minutes=N`: in memory on the daemon, the last hour at most. */
export interface HostMetricsHistory {
  host: string;
  interval_secs: number;
  minutes: number;
  /** No sample in the last 30 s, or the host is offline. */
  stale: boolean;
  /** Oldest first. */
  samples: HostMetricsSample[];
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
  /** Why the host is offline (docs/api.md §17): the last link error, an open or spent enrol token, or "never connected". Null while online. */
  offline_reason?: string | null;
  /** Live exec sessions. */
  sessions?: number;
  /** From the last `hello`. `{}` before the first connect. */
  capabilities?: HostCapabilities;
  /** `HostCapabilities::summary`: the flat view for policy checks. */
  summary?: { os: string; arch: string; docker: boolean; vmlab: boolean; display?: boolean };
  last_seen?: string | null;
  os?: string | null;
  arch?: string | null;
  /** The build version of the host agent (`0.1.0+6e31754`). */
  version?: string | null;
  /** The host agent is built from another ref than the daemon (docs/api.md §17): reinstall it. `false` when a ref is not known. */
  stale?: boolean;
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
  /** The latest performance sample; `null` with none (an older host agent, or none since the daemon started). */
  metrics?: (HostMetricsSample & { stale: boolean }) | null;
}

/** `POST /api/hosts` (`201`): the host and its token. The token shows once, here only. */
/** A link key row (docs/api.md §36): another daemon (the home) that calls this one. No hash or key is ever in it. */
export interface Link {
  id: string;
  label: string;
  /** GET, SSE, and the event WebSocket only. */
  read_only: boolean;
  /** `pending`: no key (a pairing code is open, ran out, or the link was rotated). */
  status: "pending" | "active" | "revoked";
  home_instance_id: string | null;
  home_name: string | null;
  paired_at: string | null;
  paired_from: string | null;
  created_by: string;
  created_at: string;
  rotated_at: string | null;
  first_used_at: string | null;
  last_used_at: string | null;
  last_used_from: string | null;
  pair_expires_at: string | null;
  revoked_at: string | null;
}

/** `GET /api/system/instance`. */
export interface LinkInstance {
  id: string;
  name: string;
  version: string;
}

/** The answer of `POST /api/links` and of the rotate: the one-time pairing code, shown once. */
export interface NewLinkCode {
  link: Link;
  code: string;
  code_expires_at: string;
  instance: LinkInstance;
  /** TERRARIUM_PUBLIC_URL of this daemon, or null. */
  url: string | null;
}

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
export interface AuditEntry extends LogSessionLink {
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
  /** `desc`: newest first; the cursor goes on to older rows. */
  order?: "asc" | "desc";
  limit?: number;
  cursor?: string;
}

/**
 * An `audit` frame of the firehose with `?audit=1` (docs/api.md §9.1): a
 * new or changed audit row. A live notification only: it has no `seq` of
 * the event log, and a reconnect does not replay it.
 */
export interface AuditFrame {
  kind: "audit";
  audit: AuditEntry;
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
 * `1h30m`. `at`: once, at an RFC 3339 time. `manual`: a project action
 * (TER-53, docs/api.md §19.10): no time and no event; a button runs it.
 * `day_start`: one run per day in `tz`, at the first start of the day
 * (TER-450, docs/api.md §19.3 "Start of day").
 */
export type ScheduleSpec = { kind: "cron"; expr: string; tz: string } | { kind: "interval"; every: string } | { kind: "at"; at: string } | DayStartSpec | TriggerSpec | ManualSpec;
export type ScheduleSpecKind = ScheduleSpec["kind"];
/**
 * Start of day: the daemon plans one run per calendar day in `tz` at the
 * first detected start of the day, plus `delay`, and not before
 * `not_before` (`HH:MM`). A start after `not_after` plans no run that day.
 * Empty or absent `weekdays`: every day.
 */
export interface DayStartSpec {
  kind: "day_start";
  /** Default `UTC`. */
  tz?: string;
  not_before?: string;
  not_after?: string;
  /** A duration as for `interval`, at most `12h`. */
  delay?: string;
  /** `mon` to `sun` (the daemon also takes the full names). */
  weekdays?: string[];
}
/** What the daemon saw as the start of the day. */
export type DayStartCause = "daemon_start" | "resume" | "host_reconnect" | "day_change";
/** A detected start: `day_start_last` of a schedule and `day_start` of a run. */
export interface DayStartMark {
  cause: DayStartCause | string;
  at: string;
}
/** The spec of a project action. */
export interface ManualSpec {
  kind: "manual";
}
/** The specs of a time schedule; `on` is a trigger, `manual` a project action. */
export type TimeSpec = Exclude<ScheduleSpec, TriggerSpec | ManualSpec>;

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
  | { type: "card"; op: CardOp; key: string; title?: string; body?: string; column?: string }
  | WebhookAction;
export type ScheduleActionType = ScheduleAction["type"];
export type CardOp = "open" | "close";
/** The actions a trigger may run. `turn`, `agent`, and `probe` on a trigger are `422`. */
export type TriggerActionType = Extract<ScheduleActionType, "mail" | "exec" | "card" | "webhook">;

/** The body of a webhook (docs/api.md §19.5). */
export type WebhookFormat = "json" | "slack" | "discord" | "ntfy" | "template";
export type WebhookMethod = "POST" | "PUT";

/** One request header: a fixed `value`, or the main value of the secret `secret`. */
export interface WebhookHeader {
  name: string;
  value?: string;
  secret?: string;
}

/**
 * Send an HTTP request about each event (docs/api.md §19.5). `url` (http or
 * https, no user or password) or `url_secret` (a secret whose value is the
 * URL). `template`: the body of `format: "template"`, with `{{event.kind}}`,
 * `{{event.data.status}}`, `{{trigger.name}}`, and `{{path | json}}`.
 * `timeout_secs`: default 10, at most 30.
 */
export interface WebhookAction {
  type: "webhook";
  url?: string;
  url_secret?: string;
  method?: WebhookMethod;
  format?: WebhookFormat;
  template?: string;
  content_type?: string;
  headers?: WebhookHeader[];
  signing?: { secret: string; header?: string; algo?: "sha256" };
  timeout_secs?: number;
}

/** `pending` waits for an attempt (or a retry at `next_attempt_at`); `dropped` is a rate-limit note. */
export type DeliveryStatus = "pending" | "ok" | "failed" | "giving_up" | "dropped";

/** One webhook delivery (`GET /api/schedules/{id}/deliveries`), newest first. */
export interface WebhookDelivery {
  id: string;
  schedule: string;
  event_seq: number | null;
  event_kind: string;
  /** A test send. */
  test: boolean;
  redelivery_of?: string;
  /** For the log: no credentials, no query. */
  url: string;
  method: string;
  content_type: string;
  attempt: number;
  status: DeliveryStatus;
  http_status: number | null;
  duration_ms: number | null;
  /** The start of the answer (redacted, at most 2 KiB). */
  response: string | null;
  error: string | null;
  /** `dropped`: the events that the rate limit dropped in its minute. */
  dropped?: number;
  next_attempt_at: string | null;
  created_at: string;
  updated_at: string;
  /** The start of the request body (at most 4 KiB). */
  request: string;
  request_bytes: number;
}

/** `POST /api/schedules/{id}/test`: the delivery and the request, with each secret value as `[secret:<name>]`. */
/** A sample event for `POST /api/schedules/{id}/test`: each field replaces the field of the sample of the kind. */
export interface TestEvent {
  kind?: string;
  summary?: string;
  project?: string;
  agent?: string;
  actor?: string;
  data?: Record<string, unknown>;
}

/** `POST /api/schedules/{id}/test` (docs/api.md §19.7): one test run of any action. */
export interface ScheduleTestResult {
  test: true;
  action: ScheduleAction["type"];
  /** With a given event: did it match the scope, the kinds, and each rule? `null`: not checked. */
  matched: boolean | null;
  /** Why it did not match; nothing ran. */
  reason?: string;
  /** The sample event (a trigger only). */
  event?: { kind: string; summary: string; project: string | null; data: unknown };
  /** Absent when nothing ran. */
  status?: RunStatus;
  summary?: string;
  output?: unknown;
  error?: string | null;
  /** A webhook: the delivery and the request. */
  delivery?: WebhookDelivery;
  request?: { method: string; url: string; headers: [string, string][]; body: string };
}

/** The answer of a webhook test: a {@link ScheduleTestResult} with the delivery and the request. */
export interface WebhookTestResult extends ScheduleTestResult {
  delivery: WebhookDelivery;
  request: { method: string; url: string; headers: [string, string][]; body: string };
}

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
  /** `actor` (the runs act as `run_as`), `project`, or `system` (docs/api.md §19.6). */
  owner?: ItemOwner;
  /** The actor the runs of an actor-owned schedule act as; the daemon checks its rights at each run. */
  run_as: string;
  /** Null when disabled, when a one-shot `at` schedule has run, always for a trigger and a project action, and for a `day_start` schedule while no start planned its run. */
  next_run_at: string | null;
  /** A `day_start` schedule: the local day (`YYYY-MM-DD`, in its `tz`) of its last run. Absent before the first run. */
  day_start_done?: string;
  /** A `day_start` schedule: the last detected start that planned a run. Absent before the first plan. */
  day_start_last?: DayStartMark;
  last_run: ScheduleLastRun | null;
  runs_count: number;
  /** `api`, `file`, or `fleet:<name>`. A `file` schedule changes only in its file (`409 file_managed`). */
  source: string;
  /** Rights checks in a row that skipped a run. */
  consecutive_skips?: number;
  /** Why the daemon disabled the schedule, when it did. */
  disabled_reason?: string;
  /** A webhook action: the failed attempts in a row (the 20th disables it). */
  webhook_failures?: number;
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
  /** Default `actor`. On a change, `actor` takes the schedule over (docs/api.md §19.6). */
  owner?: ItemOwner;
}

/** `PATCH /api/schedules/{id}`: a `spec` or an `action` replaces the whole object. The scope does not change. */
export type SchedulePatch = Partial<Omit<NewSchedule, "project">>;

export interface ScheduleQuery {
  project?: string;
  enabled?: boolean;
  /** `time`: only the time schedules; `trigger`: only the triggers (spec `on`); `action`: only the project actions (spec `manual`). */
  kind?: "time" | "trigger" | "action";
}

/** `cron` is the timer (each time spec); `event` is a trigger run; `day_start` the planned run of a `day_start` schedule. */
export type RunTrigger = "cron" | "manual" | "catch_up" | "event" | "day_start";

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
  /** A `day_start` run: the detected start that planned it. Absent for the other runs. */
  day_start?: DayStartMark;
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
  /** 1 at the create; one more at each change of the template's file. */
  revision: number;
  created_by: string;
  /** `config` (a WCL file in the config repo), or `fleet:<name>` (a fleet owns it; its file is in the config repo too). `api` and `file`: a template from before the files, until the first load. */
  source: "config" | Source | `fleet:${string}`;
  created_at: string;
  updated_at: string;
  /** The WCL file in the config repo: `templates/<name>.wcl` or `templates/<project>/<name>.wcl`. Null until the file is written. */
  file?: string | null;
  /** The skills that an agent of the template gets now. */
  effective_skills?: EffectiveSkill[];
  /** Only when `spec.subscription` names a subscription that does not exist: why (docs/api.md §21.1). An agent create from the template is refused. */
  subscription_missing?: string;
}

/** `GET /api/templates/{key}/wcl`: the template's WCL file (docs/api.md §21.5). */
export interface TemplateWcl {
  /** The path in the config repo. */
  path: string;
  /** The text of the file. With no file (no config repo), the text that the file would have. */
  text: string;
  /** False when the text is not from a file. */
  on_disk: boolean;
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

// ----- The config repo (PLAN §8.3, docs/api.md §25) -----

/** A problem in a fleet file. */
export interface FleetIssue {
  level: "error" | "warning";
  /** Where in the file: `projects.terrarium.agents.lead`, for example. */
  at: string;
  message: string;
}

export type FleetAction = "create" | "update" | "unchanged" | "delete" | "release" | "conflict";

/** One object of a fleet plan. */
export interface FleetPlanEntry {
  kind: "subscription" | "project" | "target" | "agent" | "schedule" | "template" | string;
  key: string;
  project?: string;
  action: FleetAction;
  /** The fields that changed outside the fleet since the last apply. */
  drift?: string[];
  adopt?: boolean;
  changes?: { field: string; from: unknown; to: unknown }[];
  reason?: string;
  [extra: string]: unknown;
}

/** A fleet file against the store: what an apply does. */
export interface FleetPlan {
  fleet: string;
  /** No errors: the plan can be applied (when it has no conflicts). */
  valid: boolean;
  errors: FleetIssue[];
  warnings: FleetIssue[];
  entries: FleetPlanEntry[];
  summary: { create: number; update: number; unchanged: number; delete: number; release: number; conflict: number; drift: number };
}

export type ConfigChangeStatus = "added" | "modified" | "deleted" | "renamed" | "untracked" | "conflict";

/** `GET /api/config/status`. */
export interface ConfigStatus {
  dir: string;
  initialized: boolean;
  branch: string;
  head?: string;
  /** `url` never holds a credential. `credential` is the name of a stored credential. */
  remote?: { url: string; credential?: string; branch: string };
  /** Commits that the remote does not have, after the last fetch. */
  ahead: number;
  /** Commits of the remote that this repo does not have, after the last fetch. */
  behind: number;
  changes: { path: string; status: ConfigChangeStatus }[];
  /** A merge stopped on a conflict, and waits. */
  merging: boolean;
  conflicts: string[];
  /** The new defaults that wait as `<file>.new`. */
  pending_defaults: string[];
  /** A pull brought `export/` changes that wait for a pull with `apply`. */
  export_pending: boolean;
  auto_sync: "off" | "pull" | "sync";
}

/** One commit of `GET /api/config/log`. */
export interface ConfigLogEntry {
  commit: string;
  author: string;
  at: string;
  subject: string;
}

/** `POST /api/config/commit`. */
export interface ConfigCommitBody {
  message: string;
  /** Write `export/` from the store first. Default true. */
  export?: boolean;
  /** Write `export/` also when a pulled export waits: the store wins. */
  force_export?: boolean;
}

export interface ConfigCommitted {
  /** Null: there was nothing to commit. */
  commit: string | null;
  files: string[];
}

export interface ConfigCommitResult extends ConfigCommitted {
  /** The export files that the commit wrote first. */
  export: string[];
}

/** The plan (or the apply) of one export file after a merge. */
export interface ConfigExportPlan {
  path: string;
  plan?: FleetPlan;
  applied?: Record<string, unknown>;
  error?: string;
}

export interface ConfigMerged {
  /** The remote has a branch to merge. */
  fetched: boolean;
  /** HEAD moved. */
  updated: boolean;
  changed: string[];
  conflicts: string[];
}

/**
 * The operator's approval of a new or wider grant of a secret that has a
 * value (S8, docs/api.md §25): `needed` in a plan, `held` when an apply
 * without `approve_grants` did not make it, `approved` when it did.
 */
export type ConfigGrantApproval = "needed" | "held" | "approved";

/** One change of the secrets import of a pulled export. No value. */
export interface ConfigSecretChange {
  action: "create" | "update" | "delete" | "grant" | "revoke";
  /** `<scope>/<name>`. */
  secret: string;
  /** A grant or a revoke: `agent:<project>/<name>` or `project:<name>`. */
  grantee?: string;
  /** Only on a grant that needs the operator. */
  approval?: ConfigGrantApproval;
  /** A grant: the level that the file gives. */
  level?: GrantLevel;
  /** A grant: the env var that the file gives. */
  env?: string;
  /** A `run` grant: the targets that the file gives. */
  targets?: string[];
  /** A grant that changes a stored grant: the stored one. */
  was?: { level: GrantLevel; env?: string; targets?: string[] };
}

/** The secrets import of a pulled export (PLAN §16b.5). */
export interface ConfigSecretsImport {
  changes: ConfigSecretChange[];
  warnings: string[];
  /** The import did not run. */
  error?: string;
}

/** `POST /api/config/pull`. */
export interface ConfigPullResult extends ConfigMerged {
  export: ConfigExportPlan[];
  /** When the export has secrets files. */
  secrets?: ConfigSecretsImport;
}

/** `POST /api/config/sync`: commit, fetch, merge, push. */
export interface ConfigSyncResult extends ConfigMerged {
  committed: ConfigCommitted;
  pushed: boolean;
  export: ConfigExportPlan[];
  /** When the export has secrets files. */
  secrets?: ConfigSecretsImport;
}

/** `PUT /api/config/remote`. A URL with a user or a password is refused (422). */
export interface ConfigRemoteBody {
  url: string;
  credential?: string;
  branch?: string;
}

/** A new default that waits as `<file>.new`, with its diff against the file. */
export interface ConfigPendingDefault {
  path: string;
  diff: string;
}

/** One hit of the secret scan, in `details.findings` of a `422`. Never the value. */
export interface ConfigScanFinding {
  path: string;
  line: number;
  rule: string;
}

// ----- Terrarium-managed skills (in the config repo) -----

/** One skill of `GET /api/skills`. */
export interface SkillSummary {
  name: string;
  /** From the front matter of SKILL.md. Empty when it does not parse. Plain text. */
  description: string;
  files: number;
  bytes: number;
  /** True when the skill is default on (`metadata.terrarium-default: "true"` in SKILL.md): each agent gets it unless it turns it off. */
  default_on?: boolean;
  /** The problems of the checks. A skill with problems is not given to agents. Absent when none. */
  errors?: string[];
}

/** One skill of the effective set of an agent or a template, and why it has it. */
export interface EffectiveSkill {
  name: string;
  /** `default`: the skill is default on. `explicit`: the `skills` list names it (or has `*`). */
  source: "default" | "explicit";
}

/** One file of a skill other than SKILL.md. */
export interface SkillFile {
  path: string;
  size: number;
  /** The text of a UTF-8 file. Absent for a binary file. */
  text?: string;
}

/** `GET /api/skills/{name}`. */
export interface SkillDetail extends SkillSummary {
  skill_md: string;
  /** Every file but SKILL.md, sorted by path. */
  file_list: SkillFile[];
}

/** The content of a file in a write: text, or base64 for a binary file. */
export type SkillFileBody = string | { base64: string };

/** `POST /api/skills`. `description` without `skill_md` makes a new SKILL.md. */
export interface NewSkill {
  name: string;
  skill_md?: string;
  description?: string;
  files?: Record<string, SkillFileBody>;
}

/** `PUT /api/skills/{name}`. `files` absent keeps the other files; present replaces them all. */
export interface SkillUpdate {
  skill_md?: string;
  files?: Record<string, SkillFileBody>;
}

/** An agent or a template that names a skill. */
export interface SkillUser {
  id: string;
  /** `project/agent` for an agent; the name (`project/name` for a project template) for a template. */
  name: string;
  /** True when the list names the skill through `"*"`. */
  all: boolean;
  /** True when the user gets the skill only because it is default on. */
  default?: boolean;
}

/** `GET /api/skills/{name}/usage`. */
export interface SkillUsage {
  agents: SkillUser[];
  templates: SkillUser[];
}

// ----- System prompts (PLAN §8.1, docs/api.md §28) -----
// The library in the config repo: `system-prompts/<name>.md`. An agent or a
// template selects an entry with `persona = name`. The text is user
// content: show it as text, never as HTML.

/** One entry of `GET /api/system-prompts`. */
export interface SystemPromptSummary {
  name: string;
  /** From the first line `<!-- description: ... -->`. Empty when none. Plain text. */
  description: string;
  /** The bytes of the file. */
  size: number;
  /** `default`: an unedited packaged default. `config`: any other file. */
  source: "default" | "config";
  /** The file in the config repo: `system-prompts/<name>.md`, or `prompts/<name>.md` in an older repo. */
  path: string;
  /** `leader` or `worker`: the persona by role. It cannot be deleted. */
  builtin: boolean;
  /** The file has more bytes than the daemon's limit (`TERRARIUM_SYSTEM_PROMPT_MAX`, default 32 KiB): an agent with this persona does not start. Absent from an older daemon. */
  over_limit?: boolean;
  /** The last change of the file (RFC 3339). */
  updated?: string;
}

/** An agent or a template that selects a system prompt. */
export interface SystemPromptUser {
  kind: "agent" | "template";
  id: string;
  /** `project/agent` for an agent; the name (`project/name` for a project template) for a template. */
  name: string;
  /** Absent for a global template. */
  project?: string;
  /** True: the agent has no persona and gets `leader` or `worker` by its roles. */
  by_role?: boolean;
}

/** `GET /api/system-prompts/{name}`. */
export interface SystemPromptDetail extends SystemPromptSummary {
  /** The prompt text, without the description line. */
  text: string;
  users: SystemPromptUser[];
}

/** `POST /api/system-prompts`. `replace` writes over an entry that exists. */
export interface NewSystemPrompt {
  name: string;
  text: string;
  description?: string;
  replace?: boolean;
}

/** `PUT /api/system-prompts/{name}`: only the fields given change. `description: ""` removes it. */
export interface SystemPromptUpdate {
  text?: string;
  description?: string;
}

// ----- Secrets (PLAN §16b, docs/api.md §5.2) -----
// No answer holds a secret field, except a reveal. The dashboard keeps a
// revealed value in the dialog only, and never in a store or in storage.

/** docs/api.md §5.2 "Kinds". */
export type SecretKind = "api_key" | "token" | "github_token" | "login" | "ssh_key" | "generic";
export const SECRET_KINDS: readonly SecretKind[] = ["api_key", "token", "github_token", "login", "ssh_key", "generic"];

/** docs/api.md §5.2 "Grants": `use` gives the brokers only; `run` also gives the value in `terrarium run` on the grant's `targets` (TER-142); `read` also gives the env var and `terrarium secret get`. */
export type GrantLevel = "use" | "run" | "read";

/** The secret shape of docs/api.md §5.2. `fields` holds the plain fields only. */
export interface Secret {
  id: string;
  /** `global` or a project name. */
  scope: string;
  name: string;
  kind: SecretKind;
  /** The plain fields: a user name, a URL, hosts, the public key and the fingerprint of an `ssh_key`. */
  fields: Record<string, unknown>;
  /** The names of the secret fields that the secret holds. */
  secret_fields: string[];
  created_at: string;
  updated_at: string;
  /** The actor label. */
  created_by: string;
}

/** `POST /api/secrets`. `fields` holds every field of the kind; `secret_fields` names the secret ones of a `generic`. */
export interface NewSecret {
  /** `global` (the default) or a project. */
  scope?: string;
  name: string;
  kind: SecretKind;
  fields: Record<string, unknown>;
  secret_fields?: string[];
}

/** `PUT /api/secrets/{scope}/{name}`. */
export interface SecretPut {
  kind: SecretKind;
  fields: Record<string, unknown>;
  secret_fields?: string[];
}

/** `POST /api/secrets/{scope}/{name}/generate`. */
export interface SecretGenerate {
  type?: "ed25519" | "rsa";
  /** `rsa` only: 3072 (the default) or 4096. */
  bits?: 3072 | 4096;
  comment?: string;
  /** Replace a secret of that name; else a name that exists is `409`. */
  replace?: boolean;
}

/** The answer of a reveal: the secret, with `values` (the secret fields by name). */
export interface RevealedSecret extends Secret {
  values: Record<string, string>;
}

/** A grant (docs/api.md §5.2 "Grants"). */
export interface SecretGrant {
  scope: string;
  name: string;
  /** The stored grantee: `agent:<ulid>` or `project:<name>`. */
  grantee: string;
  /** `agent:<project>/<name>` or `project:<name>`. */
  grantee_label: string;
  level: GrantLevel;
  env?: string;
  /** The targets of a `run` grant. */
  targets?: string[];
  created_by: string;
  created_at: string;
}

/** `POST /api/secrets/{scope}/{name}/grants`. `grantee`: `agent:<project>/<name>`, `agent:<id>`, or `project:<name>`. */
export interface NewSecretGrant {
  grantee: string;
  level?: GrantLevel;
  /** An env var name. Needs the level `read`. */
  env?: string;
}

/** `credential_info` of a target (docs/api.md §17.2). `scope` and `kind` are `null` when there is no such secret. */
export interface CredentialInfo {
  name: string;
  scope: string | null;
  kind: SecretKind | null;
  fingerprint?: string;
  warning?: string;
}

/** `PATCH /api/targets/{name}`: a JSON merge patch; `null` removes a field. */
export type TargetPatch = { [K in keyof NewTarget]?: NewTarget[K] | null };

/** The `result` of `POST /api/targets/{name}/test` (docs/api.md §17.2). */
export type TargetTestResult = "ok" | "auth_refused" | "host_key_mismatch" | "host_key_unknown" | "unreachable" | "error";

/** The answer of `POST /api/targets/{name}/test`. The key never shows. */
export interface TargetTestReport {
  target: string;
  kind: TargetKind;
  address: string;
  result: TargetTestResult;
  message: string;
  host_key: "known" | "added" | "unknown" | "mismatch" | "not_checked";
  host_key_policy: string;
  credential?: string | null;
  fingerprint?: string | null;
  exit_code: number | null;
  stderr: string;
  elapsed_ms: number;
}

// ---- test runs (docs/api.md, test runs) ----

/** The file formats of a test run. `auto` on upload detects the format from the content. */
export type TestFormat = "junit" | "trx" | "ctrf" | "libtest-json" | "vitest-json" | "jest-json" | "tap";
/** `failed`: failed + errored > 0. `passed`: one or more cases and none failed or errored. `empty`: no cases. */
export type TestRunStatus = "passed" | "failed" | "empty";
export type TestCaseStatus = "passed" | "failed" | "errored" | "skipped";

export interface TestCounts {
  passed: number;
  failed: number;
  errored: number;
  skipped: number;
  total: number;
}

/** A list item of `GET /api/projects/{p}/test-runs`, and the answer of `POST` and `PATCH .../links`. */
export interface TestRunSummary {
  /** A ULID. */
  id: string;
  project: string;
  name: string;
  format: TestFormat;
  tool: string | null;
  status: TestRunStatus;
  counts: TestCounts;
  duration_ms: number | null;
  started_at: string | null;
  commit: string | null;
  branch: string | null;
  /** The name of the original upload. */
  file_name: string;
  /** The bytes of the original upload. */
  raw_size: number;
  /** Notes of the parser, for example truncated messages. */
  notes: string[];
  /** The linked card IDs. */
  cards: string[];
  /** The linked labels, such as `version:1.4.0` (TER-58). */
  labels: string[];
  created_by: string;
  created_by_name: string;
  created_at: string;
  /** The dashboard page: `/p/<project>/tests/<id>`. Old links to `/p/<project>/board/tests/<id>` redirect there. */
  url: string;
}

export interface TestSuite {
  name: string;
  duration_ms: number | null;
  counts: TestCounts;
}

export interface TestCase {
  /** The position in the file: the paging cursor. */
  seq: number;
  suite: string;
  name: string;
  classname: string | null;
  file: string | null;
  line: number | null;
  status: TestCaseStatus;
  duration_ms: number | null;
  /** Plain text as uploaded. It can hold ANSI colour codes. */
  message: string | null;
  trace: string | null;
  stdout: string | null;
  stderr: string | null;
}

/** `GET /api/test-runs/{id}?status=&q=&limit=&cursor=`: the summary, every suite, and one page of cases in file order. */
export interface TestRun extends TestRunSummary {
  suites: TestSuite[];
  cases: TestCase[];
  /** The number of cases that match the filter (all pages). */
  cases_total: number;
  next_cursor: string | null;
}

/** `status=failed` means failed or errored. */
export interface TestRunCaseQuery {
  status?: TestCaseStatus;
  q?: string;
  limit?: number;
  cursor?: string;
}

export interface TestRunListQuery {
  card?: string;
  /** A label, such as `version:1.4.0`. */
  label?: string;
  name?: string;
  status?: TestRunStatus;
  limit?: number;
  cursor?: string;
}

/** One case in a compare. `status` is in the run; `base_status` is in the base run. */
export interface TestCaseDiff {
  suite: string;
  classname: string | null;
  name: string;
  status: TestCaseStatus | null;
  base_status: TestCaseStatus | null;
  message: string | null;
}

/** `GET /api/test-runs/{id}/compare?with=<id>|previous`. Each list has at most 1000 items; `counts` has the full numbers. */
export interface TestCompare {
  run: TestRunSummary;
  /** Null when there is no run to compare with: then every list is empty. */
  base: TestRunSummary | null;
  counts: { newly_failing: number; fixed: number; still_failing: number; new_tests: number; removed_tests: number };
  newly_failing: TestCaseDiff[];
  fixed: TestCaseDiff[];
  still_failing: TestCaseDiff[];
  new_tests: TestCaseDiff[];
  removed_tests: TestCaseDiff[];
}

/** `PATCH /api/test-runs/{id}/links`. A card is its ID; a label is free text (`version:1.4.0`). */
export interface TestRunLinksPatch {
  add_cards?: string[];
  remove_cards?: string[];
  add_labels?: string[];
  remove_labels?: string[];
}

/** `Card.tests`: the latest linked run. */
export interface CardTests {
  id: string;
  name: string;
  status: TestRunStatus;
  counts: TestCounts;
  created_at: string;
}

// ----- monitors and the project dashboard (docs/api.md §30) -----

/** The output kind of a monitor: how the daemon reads the stdout of its script. */
export type MonitorKind = "number" | "status" | "text" | "json";
/** The status of a result. `status` monitors map the exit code as Nagios does: 0 ok, 1 warn, 2 crit, other unknown. */
export type MonitorStatus = "ok" | "warn" | "crit" | "unknown";
/** Which side of a threshold is bad. */
export type MonitorDirection = "above" | "below";
/** The schedule of a monitor: an interval of 15 s or more, or a cron spec. */
export type MonitorSchedule = { kind: "interval"; every: string } | { kind: "cron"; expr: string; tz: string };

/** The fields of a monitor that a client writes. */
/**
 * Who owns a monitor or a schedule (docs/api.md §30.7): `actor` (the runs act as `run_as`), `project` (the rights
 * come from the project's targets and grants), or `system` (sysadmin or the operator).
 */
export type ItemOwner = "actor" | "project" | "system";

/** A global target granted to a project for its project-owned monitors or schedules (docs/api.md §30.7). */
export interface TargetGrant {
  target: string;
  project: string;
  for: "monitors" | "schedules";
  /** `operator` or `agent:<ulid>`. */
  granted_by: string;
  granted_at: string;
}

export interface MonitorDef {
  name: string;
  /** A write field: the owner of a new monitor, or a new owner (`actor` on a change takes it over). */
  owner?: ItemOwner;
  description?: string | null;
  /** A global target, or a target of the project. */
  target: string;
  /** Shell text; the target runs it with `sh -c`. */
  script: string;
  schedule: MonitorSchedule;
  /** 1-600; default 30. */
  timeout_secs?: number;
  kind: MonitorKind;
  unit?: string | null;
  warn?: number | null;
  crit?: number | null;
  direction?: MonitorDirection;
  enabled?: boolean;
  /** 1-90; default 7. */
  retention_days?: number;
}

/** The rows of a `json` result: one object per row, at most 200 rows of 20 columns. */
export type MonitorRow = Record<string, string | number | boolean | null>;

/** The checked object of a `json` result. */
export interface MonitorJson {
  value?: number;
  unit?: string;
  status?: MonitorStatus;
  message?: string;
  rows?: MonitorRow[];
  /** Rows, columns, or text past the caps were cut. */
  truncated?: boolean;
}

/** One result of a monitor. */
export interface MonitorResult {
  /** The actor of the run: `monitor:<project>/<name>`. */
  actor?: string;
  id: number;
  monitor: string;
  at: string;
  status: MonitorStatus;
  value?: number;
  /** The message (`status`, `json`), or the text (`text`). */
  text?: string;
  json?: MonitorJson;
  duration_ms: number;
  error?: string;
}

/** A monitor as the API shows it. */
export interface Monitor extends Required<Pick<MonitorDef, "name" | "target" | "script" | "schedule" | "kind">> {
  id: string;
  project: string;
  description: string | null;
  timeout_secs: number;
  unit: string | null;
  warn: number | null;
  crit: number | null;
  direction: MonitorDirection;
  enabled: boolean;
  retention_days: number;
  /** The seconds between two runs. A block is stale after three periods with no result. */
  period_secs: number;
  created_by: string;
  owner: ItemOwner;
  /** The actor whose rights each run of an actor-owned monitor checks again. */
  run_as: string;
  /** Why the daemon disabled it: its `run_as` agent was deleted. */
  disabled_reason?: string;
  created_at: string;
  updated_at: string;
  next_run_at: string | null;
  /** Ticks that found the previous run still running. */
  skipped: number;
  last: MonitorResult | null;
  /** A run is in progress now. */
  running: boolean;
}

/** One point of `GET /api/monitors/{id}/results?bucket=`: the mean, min, and max of a bucket, and its worst status. */
export interface MonitorPoint {
  at: string;
  status: MonitorStatus;
  value?: number;
  min?: number;
  max?: number;
  count: number;
}

export interface MonitorResultsQuery {
  /** RFC 3339, or a duration back from now (`24h`). */
  since?: string;
  until?: string;
  /** Default 500, at most 10 000: the newest. */
  limit?: number;
}

/** The raw output of a run now or a test run. Null when the command did not start (target down, refused). */
export interface MonitorOutput {
  exit_code: number | null;
  stdout: string;
  stderr: string;
}

/** The answer of a run now (`result` is kept) or a test run (nothing is kept; `result.id` is absent). */
export interface MonitorRunReport {
  result: Omit<MonitorResult, "id" | "monitor"> & { id?: number; monitor?: string };
  output: MonitorOutput | null;
}

export type DashboardBlockKind = "stat" | "status" | "chart" | "text" | "table" | "note";
/** The width of a block: 1, 2, or 4 columns of the desktop grid. */
export type DashboardBlockSize = "s" | "m" | "l";
export type DashboardRange = "1h" | "6h" | "24h" | "7d";

export interface DashboardBlock {
  id: string;
  kind: DashboardBlockKind;
  /** The monitor ID. None for a note. */
  monitor?: string;
  size: DashboardBlockSize;
  title?: string;
  /** The Markdown of a note. */
  text?: string;
  options: { range?: DashboardRange; sparkline?: boolean; decimals?: number };
}

export interface DashboardLayout {
  blocks: DashboardBlock[];
}

/** `GET /api/projects/{name}/dashboard`. */
export interface DashboardView extends DashboardLayout {
  project: string;
  updated_at: string | null;
  updated_by: string | null;
  /** Every monitor of the project, with its last result. */
  monitors: Monitor[];
}

// ---------------------------------------------------------------- pipelines

export interface EtaAccuracy {
  samples: number;
  on_time_pct: number | null;
  within_half_to_double_pct: number | null;
}

export interface EtaAccuracyView {
  raw: EtaAccuracy;
  calibrated: EtaAccuracy;
}

/** The status of a pipeline run or step (docs/api.md §31.2), the same for every adapter. */
export type PipelineRunStatus = "queued" | "running" | "waiting" | "success" | "failed" | "cancelled" | "skipped" | "unknown";

/** GitHub Actions (docs/api.md §31.3). */
export interface PipelineGithubSource {
  kind: "github";
  /** `owner/name`. */
  repo: string;
  /** One workflow: its file name (`ci.yml`) or ID. Absent: every workflow. */
  workflow?: string | null;
  branch?: string | null;
  /** The secret whose value is the token. */
  token_secret?: string | null;
  /** GitHub Enterprise Server: `https://<host>/api/v3`. */
  api_url?: string | null;
}

/** A script on a target that prints the runs as JSON (docs/api.md §31.2). */
export interface PipelineScriptSource {
  kind: "script";
  target: string;
  script: string;
  timeout_secs?: number;
  /** Environment variable → secret name. */
  env_secrets?: Record<string, string>;
}

export type PipelineSource = PipelineGithubSource | PipelineScriptSource;

/** The fields of a pipeline that a client writes. */
export interface PipelineDef {
  name: string;
  description?: string | null;
  source: PipelineSource;
  /** Default `{"kind": "interval", "every": "5m"}`; 30 s or more. */
  schedule?: MonitorSchedule;
  enabled?: boolean;
  /** The runs to keep (1-100, default 20). */
  max_runs?: number;
}

export interface PipelineStep {
  name: string;
  status: PipelineRunStatus;
  started_at?: string;
  finished_at?: string;
  url?: string;
  summary?: string;
}

/** One run of a pipeline. `id` is the adapter's ID. */
export interface PipelineRun {
  id: string;
  pipeline: string;
  name: string | null;
  status: PipelineRunStatus;
  url: string | null;
  branch: string | null;
  commit: string | null;
  actor: string | null;
  event: string | null;
  environment: string | null;
  started_at: string | null;
  finished_at: string | null;
  summary: string | null;
  /** `null`: not read yet. */
  steps: PipelineStep[] | null;
  first_seen: string;
  changed_at: string;
}

/** `GET /api/pipelines/{id}/runs/{run}`: the run, with its steps read when needed. */
export interface PipelineRunDetail extends PipelineRun {
  project: string;
  pipeline_name: string;
  adapter: PipelineSource["kind"];
  /** Why the steps did not load. */
  steps_error?: string;
}

export interface Pipeline extends Required<Pick<PipelineDef, "name" | "source" | "schedule" | "enabled" | "max_runs">> {
  id: string;
  project: string;
  description: string | null;
  period_secs: number;
  created_by: string;
  run_as: string;
  created_at: string;
  updated_at: string;
  next_run_at: string | null;
  skipped: number;
  /** The status of the newest run; `null` before the first good poll. */
  status: PipelineRunStatus | null;
  polled_at: string | null;
  ok_at: string | null;
  /** The error of the last poll; `null` when it was good. */
  error: string | null;
  latest: PipelineRun | null;
  running: boolean;
}

/** What one poll changed. */
export interface PipelinePoll {
  runs: number;
  new: number;
  changed: number;
  status: PipelineRunStatus | null;
  error: string | null;
}

/** The raw output of a script poll. */
export interface PipelineOutput {
  exit_code: number | null;
  stdout: string;
  stderr: string;
}

/** `POST /api/pipelines/{id}/refresh`. */
export interface PipelineRefresh {
  pipeline: Pipeline;
  poll: PipelinePoll;
  output: PipelineOutput | null;
}

/** `POST /api/projects/{name}/pipelines/test`: nothing is kept. */
export interface PipelineTestReport {
  at: string;
  duration_ms: number;
  runs: Omit<PipelineRun, "pipeline" | "first_seen" | "changed_at">[];
  error: string | null;
  output: PipelineOutput | null;
}

// ------------------------------------------------------------ the calendar (docs/api.md §33)

/** The kinds of a calendar event. */
export type CalendarKind = "window" | "maintenance" | "freeze" | "deploy" | "task" | "other";

/** A weekday as two letters. */
export type CalendarDay = "mo" | "tu" | "we" | "th" | "fr" | "sa" | "su";

/** A recurrence rule (an RRULE subset). The API also takes the RRULE text. */
export interface CalendarRule {
  freq: "daily" | "weekly" | "monthly";
  interval?: number;
  /** A weekly rule. Empty: the weekday of the first start. */
  weekdays?: CalendarDay[];
  /** A monthly rule: 1-31, or -1 for the last day. */
  month_day?: number;
  /** A local date (that day included) or an RFC 3339 time. */
  until?: string;
  count?: number;
  /** The zone whose wall clock repeats; absent: the event's zone. */
  tz?: string;
}

export interface CalendarLinks {
  card?: string;
  target?: string;
  repo?: string;
  url?: string;
}

export interface CalendarAssignee {
  id: string;
  name: string;
  project: string;
}

/** An event as the API shows it. `project` is null for a global event. */
export interface CalendarEvent {
  id: string;
  project: string | null;
  global: boolean;
  title: string;
  description: string;
  kind: CalendarKind;
  /** The first start and end (UTC). */
  start: string;
  end: string;
  all_day: boolean;
  /** The first and the last local day of an all-day event. */
  start_date?: string;
  end_date?: string;
  tz: string;
  recurrence: CalendarRule | null;
  rrule: string | null;
  repeat: string | null;
  links: CalendarLinks;
  assignees: CalendarAssignee[];
  /** The starts of the skipped occurrences. */
  exceptions: string[];
  created_by: string;
  created_by_name: string;
  updated_by: string;
  created_at: string;
  updated_at: string;
  /** The caller may change and remove it. */
  can_edit?: boolean;
}

/** One occurrence in a list answer. */
export interface CalendarOccurrence {
  event_id: string;
  start: string;
  end: string;
  all_day: boolean;
  title: string;
  kind: CalendarKind;
  project: string | null;
  global: boolean;
  recurring: boolean;
  /** `project/name`. */
  assignees: string[];
}

/** `GET /api/projects/{name}/calendar`, `/api/calendar`, `/api/calendar/global`. */
export interface CalendarView {
  from: string;
  to: string;
  occurrences: CalendarOccurrence[];
  events: CalendarEvent[];
  truncated: boolean;
  /** The caller may add an event to this view's calendar. */
  can_write: boolean;
}

/** The body of a create, and of a patch (every field optional; `null` clears). */
export interface CalendarInput {
  title?: string;
  description?: string;
  kind?: CalendarKind;
  /** RFC 3339; a local `2026-11-10T09:00` in `tz`; or a date of an all-day event. */
  start?: string;
  /** An all-day event: the last day. */
  end?: string | null;
  all_day?: boolean;
  tz?: string;
  recurrence?: CalendarRule | string | null;
  links?: CalendarLinks | null;
  /** Agents: an ID, `project/name`, or a name in the project. A patch replaces the list. */
  assignees?: string[];
}

export interface CalendarRange {
  from?: string;
  to?: string;
  days?: number;
  /** The combined view only. */
  project?: string;
}

export interface CalendarWatcher {
  /** The agent ULID, or `operator`. */
  watcher: string;
  name: string;
  project: string | null;
  created_by: string;
  created_at: string;
}

export interface CalendarWatchResult {
  calendar: string;
  watcher: string;
  watching: boolean;
  changed: boolean;
}

export interface CalendarGrant {
  id: string;
  name: string;
  project: string;
}

export interface CalendarGrantResult {
  agent: string;
  name: string;
  project: string;
  calendar_write: boolean;
  changed: boolean;
}

// ----- Tool proxies (GitHub #21, docs/api.md §17.11) -----

/** The rules for the agent's arguments of a tool. */
export interface ToolArgs {
  allow?: string[];
  deny_flags?: string[];
  max?: number;
  max_len?: number;
}

/** One grant of a tool: to an agent (`agent:<ulid>`) or a project (`project:<name>`). */
export interface ToolGrant {
  tool: string;
  grantee: string;
  /** `agent:<project>/<name>` or `project:<name>`. */
  grantee_label: string;
  created_by: string;
  created_at: string;
}

/**
 * A tool (`GET /api/tools`): a named command on a target that a granted
 * agent runs as a CLI, without the target. `env_secrets` holds secret
 * names, never values. `grants` only for the operator and sysadmins.
 */
export interface Tool {
  name: string;
  description?: string;
  target: string;
  command: string[];
  cwd?: string;
  timeout?: string;
  env_secrets?: Record<string, string>;
  args?: ToolArgs;
  approval?: boolean | { first: string[] };
  stdin?: boolean;
  max_output?: string;
  rate?: string;
  project?: string;
  scope: "global" | "project";
  source: "file" | "api";
  created_at: string;
  updated_at: string;
  grants?: ToolGrant[];
}


// ---- Board rules (docs/api.md §19.8, §19.9, §19.12, TER-171) ----

/** Who runs the card flow of a project (§19.11). */
export type BoardFlow = "code" | "rules";

/** One verb of a rule: `card.move`, `card.label`, ... with its arguments. */
export interface RuleAction {
  verb: string;
  args?: Record<string, unknown>;
}

/** One rule that runs (`GET /api/projects/{name}/rules/effective`). */
export interface EffectiveRule {
  name: string;
  description?: string;
  enabled: boolean;
  /** Event kind globs. */
  on: string[];
  when?: { path: string; op: string; value: unknown }[];
  if_card?: Record<string, unknown>[];
  if_job?: Record<string, unknown>[];
  max_per_hour?: number;
  actions: RuleAction[];
  line: number;
  /** `pack` (the built-in pack), `override` (a rule of rules.wcl with a pack name), or `own`. */
  origin: "pack" | "override" | "own";
  /** The rule as WCL. */
  wcl: string;
}

export interface EffectiveRules {
  project: string;
  board_flow: BoardFlow;
  /** The project has a rules.wcl. */
  exists: boolean;
  hash?: string | null;
  source?: "api" | "file" | null;
  rules: EffectiveRule[];
}

/** One finding of the lint (§19.12). */
export interface RuleFinding {
  code: "missing_column" | "stuck_column";
  column: string;
  rules: string[];
  message: string;
}

export interface RulesLint {
  project: string;
  board_flow: BoardFlow;
  source: "stored" | "text";
  findings: RuleFinding[];
}

/** One run of a rule (§19.9 run log). */
export interface RuleRun {
  id: string;
  project: string;
  rule: string;
  rule_hash: string;
  event_seq: number;
  card?: string | null;
  state: "pending" | "done" | "noop" | "failed" | "skipped";
  step: number;
  steps: { verb: string; ok: boolean; changed?: boolean; detail?: unknown; error?: string }[];
  reason?: string | null;
  created_at: string;
  updated_at: string;
}

export interface RuleToggle {
  project: string;
  rule: string;
  enabled: boolean;
  /** False: the rule was so already, rules.wcl did not change. */
  changed: boolean;
  hash?: string | null;
  /** The lint of the rules that run after the change. */
  findings?: RuleFinding[];
}

/** What the migration does with one pack rule. */
export interface RuleChange {
  rule: string;
  change: "mapped" | "disabled" | "kept";
  reasons: string[];
}

/** `POST /api/projects/{name}/rules/migrate`. */
export interface RulesMigration {
  project: string;
  applied: boolean;
  board_flow: BoardFlow;
  /** The text of rules.wcl changes. */
  changed: boolean;
  /** The new rules.wcl. */
  text: string;
  hash?: string | null;
  changes: RuleChange[];
  findings: RuleFinding[];
}
