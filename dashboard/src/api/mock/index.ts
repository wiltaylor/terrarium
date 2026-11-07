// In-memory implementation of `Api` for VITE_MOCK=1 and for tests.
// Every endpoint works. State lives in one `MockWorld` per instance.
//
// The mock follows docs/api.md: the same shapes, the same error bodies and
// codes, the same list order and cursors, and the same event frames, so a
// view that works on the mock works on the daemon. Subscriptions, quota,
// usage, and login sessions follow docs/api.md §16. Hosts, target actions,
// and exec sessions follow the daemon's new `api/hosts.rs` and
// `api/targets.rs`, which docs/api.md does not describe yet.
//
// Auth (docs/api.md §1.1). Without `client`, the mock lets every call in,
// as the Unix socket does; the unit tests use that. With `client` (the
// browser mock), the mock acts as the TCP listener: `session.create` sets
// an in-memory "cookie" (the browser keeps the real one), every call needs
// that cookie or the bearer token, a mutating call with the cookie needs
// `X-CSRF-Token` (else 403), and a socket opens without a subprotocol when
// the cookie is good.

import type { Api } from "../client";
import { mockDisplayName } from "./displayName";
import { ApiError, AuthError, isMutating, type Page } from "../http";
import { createWsEventStream, type EventStream, type RawSocket, type SocketEvent } from "../sockets";
import { mockHistory, withMetrics } from "./hostMetrics";
import { MOCK_PASSWORD, MOCK_TOKEN, mockSocket, type MockSocketAuth } from "./ws";
import { httpVerbs } from "./verbs";
import { checkKeyPlan, isZai, zaiKeyDefaults, zaiPlan, ZAI_KEY_ENV } from "./zai";
import { isMoonshot, moonshotHarnessError, moonshotKeyDefaults, moonshotPlan, moonshotProbe, MOONSHOT_KEY_ENV } from "./moonshot";
import { defaultModels, modelList, subscriptionDetail } from "./models";
import * as selfhosted from "./selfhosted";
import { splitFrontMatter } from "../../lib/markdown";
import { installCommand } from "../../lib/host-install";
import { resolveEta } from "../../lib/eta";
import { defaultTitle } from "../../features/board/columns";
import { checkColumnEdits } from "./boardColumns";
import { limitsInfo, limitsKindError, resolveLimits } from "../../lib/limits";
import { mockChanges, mockCommit, mockCommits, mockFile, mockList, mockTrees } from "./repoTrees";
import type { Question, Thread, ThreadLink } from "../types";
import type {
  Agent,
  QueuedTurn,
  AgentDef,
  Harness,
  AgentEvent,
  AgentStatus,
  Artifact,
  Block,
  Session,
  AgentScreen,
  SessionGrant,
  SessionRef,
  SessionInfo,
  TranscriptFrame,
  ArtifactAttach,
  ArtifactLinkKind,
  Memory,
  WikiPageSummary,
  AuditEntry,
  LogSessionLink,
  SelfUpdateReport,
  TerrariumRelease,
  UpdateRun,
  UpdaterImage,
  ActiveCard,
  Card,
  CardComment,
  Column,
  ColumnEdit,
  ColumnRole,
  EventQuery,
  HarnessAgent,
  HarnessReport,
  HarnessVersion,
  Host,
  Pin,
  LoginKind,
  LoginSession,
  InboxEventData,
  Message,
  Project,
  Repo,
  Role,
  HarnessUpdateResult,
  HarnessAutoUpdate,
  HarnessCheckItem,
  HarnessPinResult,
  Subscription,
  SwapResult,
  Lease,
  NewTarget,
  SnapshotQuery,
  SnapshotScope,
  Target,
  DisplayInfo,
  DisplayOpName,
  DisplayOpResult,
  DisplayShot,
  TargetReport,
  TargetSnapshot,
  TargetVerbBody,
  UsagePoint,
  UsageQuery,
  UsageTotals,
  WikiPage,
  ResetPending,
  ResetResult,
  ResetState,
} from "../types";
import { paginate, type PageQuery } from "../../lib/paging";
import { fakeUlid, MOCK_DAEMON_VERSION, seed, type MockWorld, type UsageRow } from "./data";
import { harnessUpdateKind } from "../../lib/harness-update";
import { FakePty } from "./pty";
import { createScheduleMock, type ScheduleCaller } from "./schedules";
import { createEventSourceMock } from "./eventSources";
import { createWaitMock } from "./waits";
import { createQuestionMock } from "./questions";
import { createInboxMock, isPauseNotice } from "./inbox";
import { createThreadMock } from "./threads";
import { createConfigMock } from "./config";
import { createSkillMock, skillsError } from "./skills";
import { createToolMock } from "./tools";
import { createSystemPromptMock } from "./systemPrompts";
import { createDbMock } from "./db";
import { createTestRunMock } from "./testRuns";
import { createRulesMock } from "./rules";
import { createMonitorMock } from "./monitors";
import { createPipelineMock } from "./pipelines";
import { createCalendarMock } from "./calendar";
import { createProjectFolderMock } from "./projectFolders";
import { createCardFileMock } from "./cardFiles";
import { createSpawnMock } from "./spawns";
import { applyTemplate, compactMaxAgeError, compactOnIdleError, createTemplateMock, idleStopError, personaError, resolveTemplate, systemPromptError, templateRef } from "./templates";
import { createSecretMock } from "./secrets";
import { createTranscriptStream, encodeTranscriptFrame } from "../sse";
import { appendBlock, fakeReply, mockModel, systemTurn, titleOf, type Draft as BlockDraft } from "./transcripts";

/** The daemon URL of the mock, for the host install line (the daemon uses TERRARIUM_PUBLIC_URL). */
const MOCK_PUBLIC_URL = "http://localhost:8080";

export interface MockOptions {
  /** Artificial latency for every call. Default 120 ms; tests use 0. */
  latencyMs?: number;
  /** Interval of the fake activity (tool calls, turns). 0 turns it off. Default 3000 ms. */
  eventsIntervalMs?: number;
  /** Delay before a device-code login completes by itself. Default 8000 ms. 0: never. */
  autoLoginMs?: number;
  /** Delay before a login session is `waiting` (a URL is on screen). Default 800 ms; 0 with no latency. */
  loginStepMs?: number;
  /** How long a login session lives. Default ten minutes (docs/api.md §16.4). */
  loginTtlMs?: number;
  /** Delay of each fake supervisor step (`starting` -> `idle`). Default 300 ms, like `TERRARIUM_FAKE_DELAY_MS`. */
  stepMs?: number;
  world?: MockWorld;
  now?: () => number;
  /**
   * The browser side: what the client would send. With it, the mock checks
   * auth like the TCP listener. Without it, every call is in.
   */
  client?: MockClient;
  /** False: a daemon without sessions. `session.create` gets 404, so the client answers null. Default true. */
  sessions?: boolean;
  /** How long a session lives. Default 30 days. */
  sessionTtlMs?: number;
  /** The operator password. Default `MOCK_PASSWORD`. Null: no password is set, so a password login is `409`. */
  password?: string | null;
  /**
   * Where the mock keeps its "cookie", so that a reload keeps the session as
   * a browser does. The browser mock passes sessionStorage. Default: memory only.
   */
  cookieJar?: { getItem(k: string): string | null; setItem(k: string, v: string): void; removeItem(k: string): void } | null;
  /**
   * Act as a caller that is not a sysadmin when it defines a target: a
   * `POST /api/targets` without `project` is `403 forbidden`, as the daemon
   * answers a project_admin (docs/api.md §17.2). Default false: the mock
   * caller is the operator, who may define every target.
   */
  denyGlobalTargets?: boolean;
  /** `TERRARIUM_MAX_TARGETS_PER_PROJECT` (docs/api.md §17.2). Default 8. */
  maxTargetsPerProject?: number;
  /**
   * Who calls the schedule endpoints (docs/api.md §19). Default the
   * operator, who may do everything. A member reads only; a project_admin
   * writes in its project; a global schedule needs a sysadmin. The event
   * sources (§20) and the config repo (§25: only the operator may give
   * `approve_grants`) use the same caller.
   */
  scheduleCaller?: ScheduleCaller;
  /** False: a daemon with no config repo, so `/api/config/*` answers `501 not_available`. Default true. */
  configRepo?: boolean;
  /** How long a schedule run takes before `run_finished`. Default 1500 ms with latency, else the step. */
  scheduleRunMs?: number;
}

/** What the mock reads from the client: the headers that the HTTP transport would send. */
export interface MockClient {
  /** `Authorization: Bearer`, in bearer mode. */
  token(): string | null;
  /** `X-CSRF-Token`, with a session. */
  csrf(): string | null;
  /** The transport's 401 hook. */
  onUnauthorized?(): void;
}

/** The mock's "cookie jar" key. Mock only: a real session cookie is `HttpOnly`. */
export const MOCK_COOKIE_KEY = "terrarium.mock.cookie";

const clone = <T>(v: T): T => structuredClone(v);

/** The daemon's error body and status (docs/api.md §2.5). */
/** The harnesses and the extension of the mock, as harnesses.toml has them. `npm` is the latest on npm. */
interface MockHarness {
  name: string;
  /** Null for the extension. */
  harness: HarnessVersion["harness"] | null;
  package: string;
  pinned: string;
  npm: { version: string; days: number } | { error: string };
}

const MOCK_HARNESSES: MockHarness[] = [
  { name: "claude", harness: "claude", package: "@anthropic-ai/claude-code", pinned: "2.1.310", npm: { version: "2.1.310", days: 3 } },
  // 0.x: the second number is the major.
  { name: "codex", harness: "codex", package: "@openai/codex", pinned: "0.164.0", npm: { version: "0.165.0", days: 1 } },
  { name: "copilot", harness: "copilot", package: "@github/copilot", pinned: "1.0.103", npm: { version: "1.0.110", days: 2 } },
  // The auto-update moved it past its pin; npm does not answer for it.
  { name: "pi", harness: "pi", package: "@earendil-works/pi-coding-agent", pinned: "1.4.0", npm: { error: "cannot reach npm: connection refused" } },
  { name: "copilot-prompt", harness: null, package: "copilot-custom-system-prompt", pinned: "0.1.2", npm: { version: "0.1.2", days: 30 } },
];

/** The release notes of each harness, as the daemon knows them (`harness_auto_update::changelog_url`). */
const HARNESS_CHANGELOG: Record<string, string> = {
  claude: "https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md",
  codex: "https://github.com/openai/codex/releases",
  copilot: "https://github.com/github/copilot-cli/releases",
};

const LIVE_STATUSES: string[] = ["starting", "idle", "waiting", "working", "blocked", "restarting"];

/** `x.y.z` with an optional pre-release: a version that a pin takes. */
const VERSION_RE_STRICT = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;

/** `latest`, or `x.y.z` with an optional pre-release; anything else is `422 invalid`. */
function checkHarnessVersion(v: string | undefined): string {
  const version = v ?? "latest";
  if (version !== "latest" && !/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version))
    fail(422, "invalid", `version \`${version}\` is not \`latest\` or x.y.z`);
  return version;
}

/** The screen of every mock display (docs/api.md §17.10). */
export const MOCK_SCREEN = { width: 1280, height: 800 };
/** A valid 1x1 PNG: the mock screenshot. The image stretches to the panel. */
export const MOCK_PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
const MOCK_PNG_BYTES = 70;
const MOCK_OPS: DisplayOpName[] = ["info", "screenshot", "move", "click", "drag", "scroll", "keys", "type", "ocr", "find", "record"];

export function fail(status: number, code: string, message: string, details?: unknown): never {
  const error = details === undefined ? { code, message } : { code, message, details };
  throw new ApiError(status, message, { error }, code, details);
}

/** A small line diff for the wiki mock: `-` lines left, `+` lines added. */
export function lineDiff(a: string, b: string): string {
  const la = a.split("\n");
  const lb = b.split("\n");
  return [...la.filter((l) => !lb.includes(l)).map((l) => `- ${l}`), ...lb.filter((l) => !la.includes(l)).map((l) => `+ ${l}`)].join("\n");
}

const NAME_RE = /^[a-z0-9][a-z0-9_-]{0,62}$/;
const PREFIX_RE = /^[A-Z][A-Z0-9]{0,9}$/;
const LIVE: AgentStatus[] = ["starting", "idle", "waiting", "working", "blocked", "restarting"];
/** The global idle stop of the mock daemon (`TERRARIUM_IDLE_STOP`). The mock does not stop idle agents by time. */
const MOCK_IDLE_STOP = "10m";
/** The harnesses that the daemon can compact (PLAN §18, "Auto compact"): all four. */
const MOCK_COMPACT_HARNESSES: Harness[] = ["claude", "codex", "copilot", "pi"];
/** Why pi refuses a plan login (PLAN §16.3): it uses API keys only. */
const PI_PLAN_REFUSAL = "pi uses API keys only; use Claude Code for a Claude plan and Codex for a ChatGPT plan";

/** Test and demo hooks: drive the quota policy without a timer. */
export interface MockSim {
  /** Set a window's used percent, as a probe would read it, and run the quota policy. */
  setWindow(subscription: string, window: string, usedPct: number): void;
  /** Emit `subscription.offer` for each detected login with no subscription, as the daemon does at start. */
  offerLogins(): void;
  /** The attached agent of an Inbox thread posts a message (docs/api.md §6.13). */
  threadPost(id: string, text: string): Thread;
  /** The attached agent of an Inbox thread asks a question or a bundle in it. */
  threadAsk(id: string, q: Pick<Question, "text"> & Partial<Question>): Question;
  /** The attached agent of an Inbox thread proposes its close with a handoff (TER-95). */
  threadPropose(id: string, body?: { summary?: string; links?: ThreadLink[] }): Thread;
}

/** The audit name of a terminal key, as the daemon gives it (docs/api.md §6.5); 422 for a key it refuses. */
function mockKeyName(k: string): string {
  if (k.startsWith("text:")) {
    if (k.length === 5) fail(422, "invalid", "`text:` needs the text after the colon");
    return `text(${[...k.slice(5)].length})`;
  }
  const alias: Record<string, string> = { esc: "escape", return: "enter", pgup: "pageup", pgdn: "pagedown", del: "delete" };
  const lower = k.toLowerCase().replace(/\+/g, "-");
  const parts = lower.split("-");
  const key = parts.pop()!;
  const named = ["enter", "tab", "backspace", "space", "escape", "delete", "insert", "up", "down", "left", "right", "home", "end", "pageup", "pagedown"];
  const canon = alias[key] ?? key;
  const ok = named.includes(canon) || /^[a-z0-9]$/.test(canon) || /^f([1-9]|1[0-2])$/.test(canon);
  if (!ok || parts.some((m) => !["ctrl", "alt", "shift"].includes(m))) fail(422, "invalid", `key ${JSON.stringify(k)}: a terminal has no key code for it`);
  return [...parts, canon].join("-");
}

export function createMockApi(opts: MockOptions = {}): Api & { world: MockWorld; sim: MockSim } {
  const now = opts.now ?? Date.now;
  const latency = opts.latencyMs ?? 120;
  const step = opts.stepMs ?? (latency > 0 ? 300 : 0);
  const w = opts.world ?? seed(now());
  const iso = () => new Date(now()).toISOString();
  let counter = 1000;
  const ulid = (tag: string) => fakeUlid(tag, ++counter);
  const startedAt = now();
  const later = (fn: () => void, ms = step) => setTimeout(fn, ms);
  /** `harness.update` sessions: the scripted output, one chunk for each read. */
  // The installed harness versions. An update in the mock changes them.
  const harnessInstalled: Record<string, string> = { claude: "2.1.310", codex: "0.164.0", copilot: "1.0.103", pi: "1.5.1", "copilot-prompt": "0.1.2" };
  // The version dirs on disk, and the version each agent's last start ran (an
  // agent with none runs the version of the mock's start).
  const harnessOnDisk: Record<string, string[]> = Object.fromEntries(Object.entries(harnessInstalled).map(([k, v]) => [k, [v]]));
  const harnessAtStart: Record<string, string> = { ...harnessInstalled };
  const harnessRuns: Record<string, string> = {};
  // The update policy and the last check of each harness (the daemon's `harness_updates`). Demo state: claude is
  // pinned; the auto-update moved pi to 1.5.1, so "Roll back and pin" goes to 1.4.0.
  const harnessPolicy: Record<string, Omit<HarnessAutoUpdate, "changelog">> = {};
  for (const h of ["claude", "codex", "copilot", "pi"]) harnessPolicy[h] = { policy: "auto", pinned_version: null, previous: null, latest: null, checked_at: null, result: null, detail: null };
  Object.assign(harnessPolicy.claude!, { policy: "pinned", pinned_version: "2.1.310", latest: "2.1.310", checked_at: new Date(startedAt - 6 * 3600_000).toISOString(), result: "pinned", detail: "pinned to 2.1.310; no auto-update" });
  Object.assign(harnessPolicy.pi!, { previous: "1.4.0", latest: "1.5.1", checked_at: new Date(startedAt - 6 * 3600_000).toISOString(), result: "updated", detail: "1.4.0 -> 1.5.1" });
  harnessOnDisk.pi = ["1.4.0", "1.5.1"];
  // The self-update (docs/api.md §34): the daemon runs MOCK_DAEMON_VERSION, a stable 0.2.0 is out, the updater answers.
  const mockRelease = (version: string, prerelease: boolean): TerrariumRelease => ({
    version,
    tag: `v${version}`,
    name: `v${version}`,
    prerelease,
    published_at: new Date(startedAt - 2 * 86_400_000).toISOString(),
    notes: `## What's changed\n\n- The self-update: an updater service with a backup and a rollback\n- Settings > Software shows the newest release`,
    url: `https://github.com/wiltaylor/terrarium/releases/tag/v${version}`,
  });
  const updater = {
    releases: [mockRelease("0.2.0", false), mockRelease("0.3.0-alpha", true)],
    checkedAt: new Date(startedAt - 3600_000).toISOString() as string | null,
    running: MOCK_DAEMON_VERSION.split("+")[0]!,
    current: { reference: "terrarium:dev", id: "sha256:1111", version: "0.1.0" } as UpdaterImage,
    previous: null as UpdaterImage | null,
    history: [] as UpdateRun[],
  };

  const selfUpdateLatest = () => {
    const stable = updater.releases.filter((r) => !r.prerelease);
    return stable[stable.length - 1] ?? null;
  };
  const selfUpdateReport = (): SelfUpdateReport => {
    const latest = selfUpdateLatest();
    const [cargo_version = "", git_ref = "unknown"] = MOCK_DAEMON_VERSION.split("+");
    return {
      current: { version: `${updater.running}+${git_ref}`, cargo_version: updater.running || cargo_version, git_ref, dirty: false, build_time: new Date(startedAt - 86_400_000).toISOString().replace(/\.\d+Z$/, "Z") },
      policy: "notify",
      channel: "stable",
      latest,
      update_available: !!latest && harnessUpdateKind(updater.running, latest.version) !== "none",
      checked_at: updater.checkedAt,
      check_error: null,
      updater: {
        reachable: true,
        socket: "/run/terrarium-updater/updater.sock",
        error: null,
        status: { updater_version: MOCK_DAEMON_VERSION, repository: "ghcr.io/wiltaylor/terrarium", verify: "gh", current: updater.current, previous: updater.previous, running: null, history: updater.history },
      },
      last_result: updater.history[0] ?? null,
      maintenance: null,
      drain: { on: false, since: null, by: null, busy: [] },
    };
  };
  /** The mock switch: every step passes at once. */
  const mockSwitch = (kind: "update" | "rollback", target: string, to: UpdaterImage): UpdateRun => {
    const names = kind === "update"
      ? ["inspect", "resolve", "verify", "pull", "drain", "backup", "stop", "remove", "start", "health"]
      : ["inspect", "pull", "verify", "drain", "backup", "stop", "restore", "remove", "start", "health"];
    const backup = `/state/backups/terrarium-backup-${Date.now()}.zip`;
    const run: UpdateRun = {
      id: ulid("run"),
      kind,
      actor: "operator",
      target,
      from: updater.current,
      to,
      backup,
      outcome: "succeeded",
      error: null,
      steps: names.map((name) => ({ name, state: kind === "rollback" && name === "verify" ? "skipped" : "ok", detail: name === "backup" ? `full backup ${backup}` : "", at: iso() })),
      started_at: iso(),
      ended_at: iso(),
    };
    updater.previous = updater.current;
    updater.current = to;
    updater.running = to.version ?? updater.running;
    updater.history = [run, ...updater.history].slice(0, 20);
    return run;
  };

  const reply = <T>(fn: () => T): Promise<T> =>
    new Promise<T>((resolve, reject) => {
      const run = () => {
        try {
          resolve(clone(fn()));
        } catch (e) {
          reject(e);
        }
      };
      if (latency > 0) setTimeout(run, latency);
      else queueMicrotask(run);
    });

  const notFound = (what: string): never => fail(404, "not_found", `${what} not found`);
  const noRoute = (): never => fail(404, "not_found", "no such API route");
  const find = <T>(list: T[], pred: (x: T) => boolean, what: string): T => list.find(pred) ?? notFound(what);
  const project = (name: string) => find(w.projects, (p) => p.name === name, `project \`${name}\``);
  /** The name of a repo by ULID or `project/name`: the key of the browser fixture. */
  const repoName = (id: string) => find(w.repos, (x) => x.id === id || `${x.project}/${x.name}` === id, `repo \`${id}\``).name;
  // The target as the API shows it (docs/api.md §17.2). The mock caller is the operator: `access` is `operator`, and the operator holds no lease.
  // docs/api.md §17.10: a target has a screen when it is vnc, rdp, desktop, vmlab, or docker with `display`.
  const hasDisplay = (t: Target): boolean =>
    t.kind === "vnc" || t.kind === "rdp" || t.kind === "desktop" || t.kind === "vmlab" || (t.kind === "docker" && !!t.display);
  const displayTarget = (id: string): Target => {
    const t = target(id);
    if (!hasDisplay(t)) fail(422, "invalid", `target \`${t.name}\` has no display`, { code: "bad_request" });
    if (w.hosts.find((h) => h.name === t.host)?.status !== "online") fail(409, "conflict", `host agent \`${t.host ?? "?"}\` of target \`${t.name}\` is offline`);
    return t;
  };
  const targetView = (t: Target): Target => ({
    ...t,
    has_display: hasDisplay(t),
    scope: t.project ? "project" : "global",
    host_online: t.kind === "ssh" ? null : w.hosts.some((h) => h.name === t.host && h.status === "online"),
    access: "operator",
    lease: null,
    runtime: t.runtime ?? {},
    ...(t.credential ? { credential_info: secrets.credentialInfo(t) } : {}),
    ...(t.limits ? { limits_info: limitsInfo(t.kind, t.limits) } : {}),
  });
  // The checks of the daemon's `store/targets.rs` validate (docs/api.md §17.2): a bad value is `422 invalid`.
  const targetDef = (b: NewTarget): NewTarget => {
    if (!b || typeof b.name !== "string" || !/^[a-z0-9][a-z0-9._-]*$/i.test(b.name)) fail(422, "invalid", "bad target name");
    if (!["host", "ssh", "docker", "vmlab", "vnc", "rdp", "desktop"].includes(b.kind)) fail(422, "invalid", "`kind` must be host, ssh, docker, vmlab, vnc, rdp, or desktop");
    for (const f of ["host", "address", "credential", "image", "container", "machine", "lab_ref", "username", "domain"] as const) {
      const v = b[f];
      if (v !== undefined && (typeof v !== "string" || !v || /\s/.test(v) || v.startsWith("-"))) fail(422, "invalid", `bad \`${f}\` \`${String(v)}\`: not empty, no spaces, no leading \`-\``);
    }
    const p = b.lab_path;
    if (p !== undefined && (typeof p !== "string" || p.startsWith("/") || p.split("/").some((x) => x === ".." || x.startsWith("-")) || /\s/.test(p)))
      fail(422, "invalid", `bad \`lab_path\` \`${String(p)}\`: a relative directory with no \`..\``);
    const need = (f: string, ok: boolean) => ok || fail(422, "invalid", `a \`${b.kind}\` target needs \`${f}\``);
    if (b.kind === "host") need("host", !!b.host);
    if (b.kind === "ssh") need("address", !!b.address);
    if (b.kind === "vnc" || b.kind === "rdp") {
      need("host", !!b.host);
      need("address", !!b.address);
    }
    if (b.kind === "desktop") need("host", !!b.host);
    if ((b.display || b.display_name) && b.kind !== "docker" && b.kind !== "desktop")
      fail(422, "invalid", `\`display\` and \`display_name\` are for a docker or desktop target, not \`${b.kind}\``);
    if (b.username && b.kind !== "vnc" && b.kind !== "rdp") fail(422, "invalid", `\`username\` is for a vnc or rdp target, not \`${b.kind}\``);
    if (b.domain && b.kind !== "rdp") fail(422, "invalid", `\`domain\` is for an rdp target, not \`${b.kind}\``);
    if (b.kind === "docker") {
      need("host", !!b.host);
      need("image` or `container", !!(b.image || b.container));
    }
    if (b.kind === "vmlab") {
      need("host", !!b.host);
      // docs/api.md §17.2: one of `lab`, `lab_repo`, and `lab_from_workspace`.
      if ([!!b.lab, !!b.lab_repo, !!b.lab_from_workspace].filter(Boolean).length !== 1)
        fail(422, "invalid", "a `vmlab` target needs one of `lab` (a lab dir on the host), `lab_repo` (a repo of the project), and `lab_from_workspace = true`");
      if ((b.lab_repo || b.lab_from_workspace) && !b.project) fail(422, "invalid", "`lab_repo` and `lab_from_workspace` need a project target");
      if (b.lab_from_workspace && b.snapshot_before_task) fail(422, "invalid", "`snapshot_before_task` does not work with `lab_from_workspace`: each holder has its own instance");
      if (!b.lab && !b.machine) fail(422, "invalid", "`lab_repo` and `lab_from_workspace` need `machine`: the machine that runs the commands and gets the workspace");
      if (b.lab && (b.lab_ref || b.lab_path)) fail(422, "invalid", "`lab_ref` and `lab_path` go with `lab_repo` or `lab_from_workspace`");
      if (b.lab && !b.lab.startsWith("/")) fail(422, "invalid", "`lab` must be an absolute path");
    } else if (b.lab_repo || b.lab_ref || b.lab_path || b.lab_from_workspace) {
      fail(422, "invalid", `\`lab_repo\`, \`lab_ref\`, \`lab_path\`, and \`lab_from_workspace\` are for a vmlab target, not \`${b.kind}\``);
    }
    if (b.ephemeral && !(b.kind === "vmlab" || (b.kind === "docker" && b.image))) fail(422, "invalid", "an ephemeral target must be docker with `image`, or vmlab");
    // docs/api.md §17.2, "Resource limits": `{}` is no limits.
    if (b.limits && !Object.keys(b.limits).length) delete b.limits;
    if (b.limits) {
      const kindError = limitsKindError(b.kind, !!b.image);
      if (kindError) fail(422, "invalid", kindError);
      const r = resolveLimits(b.limits);
      if ("error" in r) fail(422, "invalid", r.error);
    }
    const def: NewTarget = { name: b.name, kind: b.kind };
    for (const [k, v] of Object.entries(b) as [keyof NewTarget, unknown][]) if (v !== undefined && v !== null && v !== "" && v !== false) (def as unknown as Record<string, unknown>)[k] = v;
    return def;
  };
  // docs/api.md §17.3: the snapshot verbs. vmlab keeps each snapshot per machine; `scope` picks the target's
  // machine, or every machine of the lab (the mock lab has the target's machine only, else `vm1`).
  const CLEAN = "terrarium-clean";
  const snapScope = (t: Target, s?: SnapshotScope): SnapshotScope => s ?? (t.machine ? "machine" : "lab");
  const scopeMachines = (t: Target, scope: SnapshotScope): string[] => {
    if (scope === "lab") return [t.machine ?? "vm1"];
    if (!t.machine) fail(422, "invalid", `target \`${t.name}\` has no \`machine\`: use the scope \`lab\``);
    return [t.machine!];
  };
  const checkSnapName = (n: string) => {
    if (!n || n.startsWith("-") || /[\s\p{Cc}]/u.test(n)) fail(422, "invalid", `bad snapshot name \`${n}\``);
  };
  /** Take or restore on the host rows of one instance (`key`). Answers the snapshot name and the scope. */
  const snapVerb = (t: Target, key: string, verb: "snapshot" | "restore", body?: TargetVerbBody): { name: string; scope: SnapshotScope } => {
    if (body?.name !== undefined) checkSnapName(body.name);
    const name = body?.name ?? "terrarium";
    const scope = snapScope(t, body?.scope);
    const machines = scopeMachines(t, scope);
    const rows = (w.hostSnapshots[key] ??= []);
    if (verb === "snapshot") {
      for (const machine of machines) if (!rows.some((r) => r.name === name && r.machine === machine)) rows.push({ name, kind: t.state === "running" ? "online" : "offline", taken: iso(), machine });
      if (!t.snapshots.includes(name)) t.snapshots.push(name);
    } else {
      const missing = machines.find((m) => !rows.some((r) => r.name === name && r.machine === m));
      if (missing) fail(422, "invalid", `vmlab.snapshot_restore failed: exit 1`, { verb: "vmlab.snapshot_restore", code: "failed", end: { type: "exit", code: 1, reason: "exited" }, stderr: `"${missing}" has no snapshot "${name}"`, stdout: "" });
    }
    return { name, scope };
  };
  /** The target and instance of a snapshot list or delete: a vmlab target whose host is online; a `lab_from_workspace` target needs `agent`. */
  const snapTarget = (id: string, q: SnapshotQuery): { t: Target; key: string; holder?: string } => {
    const t = target(id);
    if (t.kind !== "vmlab") fail(422, "invalid", `a \`${t.kind}\` target has no \`snapshot\``);
    if (w.hosts.find((x) => x.name === t.host)?.status !== "online") fail(409, "conflict", `host agent \`${t.host ?? "?"}\` of target \`${t.name}\` is offline`);
    if (!t.lab_from_workspace) return { t, key: t.name };
    if (!q.agent) fail(422, "invalid", `\`${t.name}\` gives each holder its own lab instance: name the holder with \`agent\``);
    const a = agent(q.agent!);
    if (!t.runtime?.instances?.[a.id]) fail(409, "conflict", `target \`${t.name}\` has no lab instance on its host; bring it up first (POST /api/targets/${t.name}/up)`);
    return { t, key: `${t.name}.${a.id}`, holder: `${a.project}/${a.name}` };
  };

  // docs/api.md §17.3, §17.8: a verb on a `lab_from_workspace` target acts on the instance of one holder (`agent`).
  const instanceVerb = (t: Target, verb: string, key: string | undefined, online: boolean, body?: TargetVerbBody): TargetReport => {
    if (!key) fail(422, "invalid", `\`${t.name}\` gives each holder its own lab instance: name the holder with \`agent\``);
    const a = agent(key!);
    if (!online) fail(409, "conflict", `host agent \`${t.host ?? "?"}\` of target \`${t.name}\` is offline`);
    const instances = { ...(t.runtime?.instances ?? {}) };
    const cur = instances[a.id];
    if (verb === "snapshot" || verb === "restore") {
      if (!cur || cur.state !== "running") fail(409, "conflict", `\`${t.name}\` has no instance of \`${a.project}/${a.name}\` on the host: bring it up first`);
      const s = snapVerb(t, `${t.name}.${a.id}`, verb, body);
      const r: TargetReport = { target: t.name, verb, state: cur!.state, snapshot: s.name, scope: s.scope };
      audit(`target.${verb}`, t.name, t.project, { ...r, agent: a.id });
      return r;
    }
    if (verb === "up" && cur?.state !== "running") {
      checkRunningCap(t, a.id);
      const ws = `${a.project}/${a.name}`;
      instances[a.id] = { agent_name: ws, lab: `/var/lib/terrarium-host/instances/${t.name}.${a.id}/lab`, state: "running", attached: [ws] };
    } else if (verb === "down" && cur) {
      if (t.ephemeral && !t.keep) delete instances[a.id];
      else instances[a.id] = { ...cur, state: "stopped" };
    }
    t.runtime = { ...t.runtime, instances };
    t.state_at = iso();
    const state = instances[a.id]?.state ?? "absent";
    const r: TargetReport = { target: t.name, verb, state };
    audit(`target.${verb}`, t.name, t.project, { ...r, agent: a.id });
    return r;
  };
  // docs/api.md §17.3: the docker and vmlab targets (and lab instances) that run on a host, without the one that starts.
  const runningOn = (host: string, except: { target: string; agent?: string }) => {
    let n = 0;
    for (const t of w.targets) {
      if (t.host !== host || (t.kind !== "docker" && t.kind !== "vmlab")) continue;
      if (t.lab_from_workspace) n += Object.entries(t.runtime?.instances ?? {}).filter(([k, i]) => i.state === "running" && !(t.name === except.target && k === except.agent)).length;
      else if (t.state === "running" && t.name !== except.target) n += 1;
    }
    return n;
  };
  const checkRunningCap = (t: Target, agent?: string) => {
    const h = w.hosts.find((x) => x.name === t.host);
    const max = h?.max_running_targets;
    if (!h || max === undefined) return;
    const running = runningOn(h.name, { target: t.name, agent });
    if (running >= max) fail(429, "rate_limited", `host \`${h.name}\` runs ${running} targets, and its max_running_targets is ${max}; bring one down first`);
  };

  /** An agent key: a ULID, `project/name`, or a unique bare name (docs/api.md §2.2). */
  const agent = (key: string): Agent => {
    const byId = w.agents.find((a) => a.id === key);
    if (byId) return byId;
    if (key.includes("/")) {
      const [p, n] = key.split("/", 2);
      return find(w.agents, (a) => a.project === p && a.name === n, `agent \`${key}\``);
    }
    const named = w.agents.filter((a) => a.name === key);
    if (named.length > 1) fail(400, "bad_request", `agent name \`${key}\` is in more than one project; use project/name`);
    return named[0] ?? notFound(`agent \`${key}\``);
  };
  const cardById = (id: string) => find(w.cards, (c) => c.id.toUpperCase() === id.toUpperCase(), `card ${id}`);
  const sub = (name: string) => find(w.subscriptions, (s) => s.name === name, `subscription ${name}`);

  // ----- addresses (docs/api.md §2.3) -----
  type Addr = { addr: string; kind: Message["from_kind"]; label: string; project?: string };
  const address = (raw: string, senderProject?: string): Addr => {
    if (raw === "operator") return { addr: "operator", kind: "operator", label: "operator" };
    if (raw === "system") return { addr: "system", kind: "system", label: "system" };
    if (raw.startsWith("project:")) {
      const p = project(raw.slice(8));
      return { addr: `project:${p.name}`, kind: "project", label: `project:${p.name}`, project: p.name };
    }
    const local = senderProject && !raw.includes("/") ? w.agents.find((a) => a.project === senderProject && a.name === raw) : undefined;
    const a = local ?? agent(raw);
    return { addr: a.id, kind: "agent", label: `${a.project}/${a.name}`, project: a.project };
  };

  // ----- audit (docs/api.md §4) -----
  /** The `audit` frames of the firehose with `?audit=1` (docs/api.md §9.1). The mock caller is the operator: every row. */
  const auditHub = new Set<(a: AuditEntry) => void>();
  const audit = (action: string, target: string, proj: string | undefined, detail: Record<string, unknown> = {}, actor = "operator") => {
    const seq = (w.audit.at(-1)?.seq ?? 0) + 1;
    const row: AuditEntry = { id: ulid("aud"), seq, at: iso(), actor, action, target, ...(proj ? { project: proj } : {}), detail };
    w.audit.push(row);
    auditHub.forEach((fn) => fn(clone(row)));
    return row;
  };

  // ----- secrets (PLAN §16b, docs/api.md §5.2): ./secrets.ts -----
  // The operator password is read at each reveal; it is declared with the session below.
  const secrets = createSecretMock({ w, iso, ulid, reply, fail, audit, password: () => password, caller: opts.scheduleCaller });

  // ----- event log and live hub (docs/api.md §9) -----
  const hub = new Set<(e: AgentEvent) => void>();
  type Draft = Omit<AgentEvent, "seq" | "id" | "at" | "data"> & { data?: Record<string, unknown> };
  const emit = (d: Draft): AgentEvent => {
    w.seq++;
    const data = { ...(d.data ?? {}) };
    if (d.agent_id) data.agent_id ??= d.agent_id;
    if (d.card) data.card_id ??= d.card;
    const e: AgentEvent = { seq: w.seq, id: fakeUlid("ev", w.seq), at: iso(), actor: "system", ...d, data };
    w.events.push(e);
    if (w.events.length > 2000) w.events.splice(0, w.events.length - 2000);
    hub.forEach((fn) => fn(clone(e)));
    return e;
  };
  // ----- session transcripts (docs/api.md §6, transcripts) -----
  const transcriptHub = new Map<string, Set<(f: TranscriptFrame) => void>>();
  const blocksOf = (sessionId: string): Block[] => (w.transcripts[sessionId] ??= []);
  const frame = (sessionId: string, f: TranscriptFrame) => transcriptHub.get(sessionId)?.forEach((fn) => fn(clone(f)));
  const currentSession = (a: Agent): Session | undefined => w.sessions.find((x) => x.agent === a.id && x.current);
  /** The harness wrote a new line: a new block at the end of the transcript. */
  const writeBlock = (a: Agent, d: BlockDraft): Block | undefined => {
    const s = currentSession(a);
    if (!s) return undefined;
    const b = appendBlock(blocksOf(s.id), s, a.harness, d, iso());
    frame(s.id, { type: "block", block: b });
    return b;
  };
  /** The harness streamed more of a block: the same seq, more text. */
  const growBlock = (sessionId: string, seq: number, text: string) => {
    const b = blocksOf(sessionId).find((x) => x.seq === seq);
    if (!b) return;
    b.text = text;
    frame(sessionId, { type: "update", block: b });
  };
  const sessionView = (s: Session): Session => {
    const a = w.agents.find((x) => x.id === s.agent);
    const list = w.transcripts[s.id];
    return { ...s, transcript: list !== undefined, blocks: list?.length ?? 0, title: titleOf(list), active: s.current && !!a && LIVE.includes(a.status) };
  };
  /** A session as a link (docs/api.md §6.6): its agent, or the tombstone of a deleted one. */
  const sessionRef = (id: string): SessionRef | undefined => {
    const s = w.sessions.find((x) => x.id === id);
    if (!s) return undefined;
    const a = w.agents.find((x) => x.id === s.agent);
    const gone = a ? undefined : w.removedAgents.find((x) => x.id === s.agent);
    const who = a ?? gone;
    if (!who) return undefined;
    return { id: s.id, agent_id: s.agent, agent: who.name, project: who.project, harness: who.harness, started_at: s.started_at, ...(s.ended_at ? { ended_at: s.ended_at } : {}), removed: !a };
  };
  /**
   * The session link of a log row (store/session_refs.rs `links`): the
   * recorded session, else the agent's session whose window holds `at`
   * (the latest start wins). The mock caller is the operator, and the mock
   * keeps every archive: each found session links.
   */
  const withSession = <T extends LogSessionLink>(row: T, agentId: string | undefined, at: string, recorded?: string): T => {
    const s = recorded
      ? w.sessions.find((x) => x.id === recorded)
      : agentId
        ? w.sessions.filter((x) => x.agent === agentId && x.started_at <= at && (!x.ended_at || x.ended_at >= at)).sort((x, y) => y.started_at.localeCompare(x.started_at))[0]
        : undefined;
    const r = s && sessionRef(s.id);
    if (!r) return row;
    return { ...row, session_id: r.id, session_label: `${r.agent} · ${r.started_at.slice(0, 16).replace("T", " ")} UTC` };
  };
  const eventLink = (e: AgentEvent): AgentEvent => withSession(e, e.agent_id, e.at, e.session);
  const mailLink = (m: Message): Message => withSession(m, m.from_kind === "agent" ? m.from : undefined, m.created_at);
  const auditLink = (a: AuditEntry): AuditEntry => withSession(a, a.actor.startsWith("agent:") ? a.actor.slice(6) : undefined, a.at);
  /** A fake `text/event-stream` body: the blocks after `after`, then live frames until the client aborts. */
  const transcriptBody = (sessionId: string, after: number | undefined, signal: AbortSignal): AsyncIterable<string> => {
    const queue: string[] = [];
    let wake: (() => void) | null = null;
    let done = false;
    const push = (chunk: string) => {
      queue.push(chunk);
      wake?.();
    };
    for (const b of blocksOf(sessionId).filter((x) => x.seq > (after ?? 0))) push(encodeTranscriptFrame({ type: "block", block: clone(b) }));
    const fn = (f: TranscriptFrame) => push(encodeTranscriptFrame(f));
    const subs = transcriptHub.get(sessionId) ?? new Set();
    transcriptHub.set(sessionId, subs);
    subs.add(fn);
    const stop = () => {
      done = true;
      subs.delete(fn);
      wake?.();
    };
    signal.addEventListener("abort", stop, { once: true });
    return {
      async *[Symbol.asyncIterator]() {
        try {
          while (!done) {
            if (queue.length) {
              // Split one frame in two, as a network would.
              const chunk = queue.shift()!;
              const cut = Math.floor(chunk.length / 2);
              yield chunk.slice(0, cut);
              yield chunk.slice(cut);
              continue;
            }
            await new Promise<void>((r) => (wake = r));
            wake = null;
          }
        } finally {
          stop();
        }
      },
    };
  };

  // ----- harness versions and updates into version dirs (docs/api.md §4) -----
  /** The live agents on a harness and the version each runs. */
  const harnessAgents = (harness: string): HarnessAgent[] =>
    w.agents.filter((a) => a.harness === harness && LIVE_STATUSES.includes(a.status)).map((a) => ({ agent: `${a.project}/${a.name}`, id: a.id, version: harnessRuns[a.id] ?? harnessAtStart[harness] ?? null }));
  const harnessReport = (latest: boolean): HarnessReport => {
    const at = new Date(startedAt).toISOString();
    const rows = MOCK_HARNESSES.filter((h): h is MockHarness & { harness: HarnessVersion["harness"] } => h.harness !== null).map((h) => {
      const installed = harnessInstalled[h.name] ?? null;
      const agents = harnessAgents(h.harness);
      const base: HarnessVersion = {
        harness: h.harness,
        package: h.package,
        base: h.pinned,
        pinned: h.pinned,
        installed,
        checked_at: at,
        ...(installed ? { active: installed, versions: [...(harnessOnDisk[h.name] ?? [])] } : {}),
        ...(agents.length ? { agents } : {}),
        auto_update: harnessAutoUpdate(h.name, installed),
      };
      if (!latest) return base;
      if ("error" in h.npm) return { ...base, latest: null, update: null, latest_error: h.npm.error };
      return { ...base, latest: { version: h.npm.version, published_at: new Date(startedAt - h.npm.days * 86_400_000).toISOString() }, update: harnessUpdateKind(installed ?? h.pinned, h.npm.version) };
    });
    return { harnesses: rows, auto_update: { schedule: "daily", next_check_at: new Date(startedAt + 5 * 60_000).toISOString() } };
  };
  /** The rollback target: the version before the last switch when it is on disk, else the newest dir under the active one. */
  const harnessPrevious = (name: string, active: string | null): string | null => {
    const p = harnessPolicy[name]?.previous ?? null;
    const disk = harnessOnDisk[name] ?? [];
    if (p && p !== active && disk.includes(p)) return p;
    if (!active) return null;
    return [...disk].reverse().find((v) => harnessUpdateKind(v, active) !== "none") ?? null;
  };
  const harnessAutoUpdate = (name: string, active: string | null): HarnessAutoUpdate => {
    const p = harnessPolicy[name]!;
    return { ...p, previous: harnessPrevious(name, active), changelog: HARNESS_CHANGELOG[name] ?? null };
  };
  /** After a switch: `from` is the previous version, and a pinned harness is pinned to `to`. */
  const recordSwitch = (name: string, from: string | null, to: string) => {
    const p = harnessPolicy[name];
    if (!p) return;
    if (from && from !== to) p.previous = from;
    if (p.policy === "pinned") p.pinned_version = to;
  };
  const updateHarness = (h: MockHarness, version: string, forced: boolean, how: "manual" | "auto" | "pin" = "manual"): HarnessUpdateResult => {
    const target = version === "latest" ? ("error" in h.npm ? null : h.npm.version) : version;
    const from = harnessInstalled[h.name] ?? null;
    if (target === null) fail(422, "invalid", `npm install -g ${h.package}@latest failed`, { output: "npm error code ECONNREFUSED\nnpm error network request failed", package: h.package, version, from });
    if (target.startsWith("99."))
      fail(422, "invalid", `npm install -g ${h.package}@${target} failed (exit 1)`, {
        output: `npm error code ETARGET\nnpm error notarget No matching version found for ${h.package}@${target}.\n`,
        package: h.package,
        version: target,
        from,
      });
    const disk = harnessOnDisk[h.name] ?? [];
    const reused = disk.includes(target) && !forced;
    harnessInstalled[h.name] = target;
    if (h.harness) recordSwitch(h.name, from, target);
    // Running agents keep their version; the GC keeps the versions they run, the previous and the pinned version,
    // and the newest two under the active one.
    const old = h.harness ? harnessAgents(h.harness).filter((a) => a.version !== target) : [];
    const all = (disk.includes(target) ? disk : [...disk, target]).sort((a, b) => (harnessUpdateKind(a, b) === "none" ? (a === b ? 0 : 1) : -1));
    const below = all.filter((v) => harnessUpdateKind(v, target) !== "none").slice(-2);
    const pol = harnessPolicy[h.name];
    const used = new Set([target, ...below, ...(pol?.previous ? [pol.previous] : []), ...(pol?.pinned_version ? [pol.pinned_version] : []), ...old.map((a) => a.version).filter((v): v is string => v !== null)]);
    const removed = all.filter((v) => !used.has(v));
    harnessOnDisk[h.name] = all.filter((v) => used.has(v));
    const names = old.map((a) => a.agent);
    if (how === "manual") {
      audit("harness.update", h.name, undefined, { from, to: target, version, force: forced, reused, agents_on_old_version: names });
      emit({ kind: "harness", actor: "operator", summary: `${h.name} ${from ?? "-"} -> ${target}`, data: { change: "updated", harness: h.name, package: h.package, from, to: target, forced, agents_on_old_version: names } });
    }
    return {
      harness: h.name,
      package: h.package,
      from,
      to: target,
      output: reused ? `${h.package}@${target} is complete on disk: no npm run\n` : `\nadded 1 package in 3s\n`,
      reused,
      agents_on_old_version: old,
      ...(old.length ? { note: `${old.length} live agent(s) run another version of ${h.name}: ${names.join(", ")}. Each keeps its version and moves to ${target} at its next restart` } : {}),
      ...(removed.length ? { removed } : {}),
    };
  };

  const agentEvent = (a: Agent, kind: AgentEvent["kind"], summary: string, data: Record<string, unknown> = {}, actor = "system") =>
    emit({ kind, actor, agent: a.name, agent_id: a.id, project: a.project, session: w.sessions.find((s) => s.agent === a.id && s.current)?.id, summary, data });
  /** Set below (resets): a reset that waits runs when the agent comes to rest. */
  let onRest: (a: Agent) => void = () => {};
  const setStatus = (a: Agent, status: AgentStatus, reason: string | null = null) => {
    const from = a.status;
    if (from === status) return;
    a.status = status;
    if (reason) a.status_reason = reason;
    else delete a.status_reason;
    a.updated_at = iso();
    agentEvent(a, "status", `${a.name}: ${from} -> ${status}`, { status, from, reason, ...(a.waits ? { waits: a.waits, waiting_on: a.waiting_on } : {}) });
    const cur = currentSession(a);
    if (cur) frame(cur.id, { type: "status", status });
    waitMock.statusChanged(a, status);
    if (status === "idle" || status === "waiting") onRest(a);
  };
  // ----- waits and watches -----
  // The skill library lives in the config repo: no repo, no skills.
  // The project DB (docs/api.md §27): ./db. The seed fills project `terrarium`.
  // Its own counter: the seed does not move the IDs of the other mock objects.
  let dbCounter = 0;
  const dbMock = createDbMock({ iso, ulid: (tag) => fakeUlid(tag, ++dbCounter), reply, fail, emit, audit, project: (name) => project(name), now: now() });
  if (w.projects.some((x) => x.name === "terrarium")) dbMock.seed();
  // Test runs (docs/api.md, test runs): ./testRuns. The seed gives project `terrarium` a run in each format.
  const testRunMock = createTestRunMock({
    reply,
    fail,
    emit,
    audit,
    project: (name) => project(name),
    cards: () => w.cards,
    agentId: (name) => w.agents.find((a) => a.project === "terrarium" && a.name === name)?.id ?? "operator",
    now: now(),
  });
  if (w.projects.some((x) => x.name === "terrarium") && w.cards.some((c) => c.id === "TER-12")) testRunMock.seed();
  // The board rules (docs/api.md §19.12): ./rules. The seed gives project `terrarium` two runs.
  const rulesMock = createRulesMock({ reply, fail, audit, project: (name) => project(name), columns: (name) => w.columns[name] ?? [], iso });
  if (w.projects.some((x) => x.name === "terrarium")) rulesMock.seed();
  // The files of the cards (docs/api.md §8.4): ./cardFiles. The seed gives one assigned card a brief (two versions), a JSON file, and an image.
  const cardFileMock = createCardFileMock({ iso, reply, fail, emit, audit, card: (id) => cardById(id) });
  cardFileMock.seed(w.cards);
  // Monitors and the project dashboard (docs/api.md §30): ./monitors. The seed gives project `terrarium` a monitor of
  // each kind with a week of history; the activity timer runs the due ones. Its own counter, as the DB mock.
  let monitorCounter = 0;
  const monitorMock = createMonitorMock({ reply, fail, emit, audit, project: (name) => project(name), targets: () => w.targets, caller: opts.scheduleCaller, ulid: (tag) => fakeUlid(tag, ++monitorCounter), now });
  if (w.projects.some((x) => x.name === "terrarium")) monitorMock.seed();
  // Pipelines (docs/api.md §31): ./pipelines. The seed gives project `terrarium` a GitHub pipeline, an Octopus Deploy
  // script pipeline, and one that fails; the activity timer polls the due ones.
  let pipelineCounter = 0;
  const pipelineMock = createPipelineMock({ reply, fail, emit, audit, project: (name) => project(name), caller: opts.scheduleCaller, ulid: (tag) => fakeUlid(tag, ++pipelineCounter), now });
  if (w.projects.some((x) => x.name === "terrarium")) pipelineMock.seed();
  // Project folders of the sidebar (docs/api.md §6.12): ./projectFolders. No seed.
  let folderCounter = 0;
  const folderMock = createProjectFolderMock({
    reply,
    fail,
    audit,
    project: (name) => project(name),
    caller: opts.scheduleCaller,
    ulid: (tag) => fakeUlid(tag, ++folderCounter),
    iso,
  });
  // The calendar (docs/api.md §33): ./calendar. The seed gives project `terrarium` a weekly maintenance window, a freeze,
  // a deploy, and a monthly task, and the global calendar a host window. Watcher mail comes from `system`.
  let calendarCounter = 0;
  const calendarMock = createCalendarMock({
    reply,
    fail,
    emit,
    audit,
    project: (name) => project(name),
    agents: () => w.agents,
    mail: (to, subject, body) => sendMail(to, "system", subject, body),
    caller: opts.scheduleCaller,
    ulid: (tag) => fakeUlid(tag, ++calendarCounter),
    now,
  });
  if (w.projects.some((x) => x.name === "terrarium") && w.agents.some((a) => a.name === "lead")) calendarMock.seed();
  const skillMock = createSkillMock({ w, reply, fail, audit, caller: opts.scheduleCaller, available: opts.configRepo });
  const toolMock = createToolMock({ w, iso, reply, fail, audit });
  // The effective skills of an agent (PLAN §8.6), as the API fills them.
  const withSkills = (a: Agent): Agent => Object.assign(a, { effective_skills: skillMock.effective(a) });
  const systemPromptMock = createSystemPromptMock({ w, iso, reply, fail, audit, emit, caller: opts.scheduleCaller, available: opts.configRepo });
  const waitMock = createWaitMock({ w, iso, now, ulid, reply, fail, emit, agent, setStatus, sendMail: (to, from, subject, body) => sendMail(to, from, subject, body) });
  // ----- ephemeral agents (docs/api.md §6.10): ./spawns.ts -----
  /** `agent rm` of one agent: the tombstone, the ended sessions, and `agent_removed`. */
  const removeAgentRow = (a: Agent) => {
    if (LIVE.includes(a.status)) setStatus(a, "stopped", "stopped by operator");
    w.agents = w.agents.filter((x) => x.id !== a.id);
    // The foreign key of `pinned_agents` removes the pin.
    w.pins = w.pins.filter((p) => p.agent !== a.id);
    // The sessions stay and end; the tombstone names the agent (docs/api.md §6.6).
    w.removedAgents.push({ id: a.id, project: a.project, name: a.name, harness: a.harness });
    for (const s of w.sessions.filter((x) => x.agent === a.id)) {
      s.current = false;
      s.ended_at ??= iso();
      s.outcome ??= "removed";
    }
    audit("agent.delete", a.id, a.project, { name: a.name });
    emit({ kind: "agent_removed", actor: "operator", agent: a.name, agent_id: a.id, project: a.project, summary: `${a.project}/${a.name}: removed`, data: { agent_id: a.id, project: a.project, name: a.name } });
  };
  const spawnMock = createSpawnMock({
    w,
    iso,
    now,
    ulid,
    reply,
    fail,
    emit,
    audit,
    agent,
    setStatus,
    sendMail: (to, from, subject, body) => sendMail(to, from, subject, body),
    startWithTurn: (a, text) => {
      openSession(a);
      setStatus(a, "starting");
      later(() => {
        setStatus(a, waitMock.restStatus(a));
        deliverTurn(a, text);
      });
    },
    removeAgent: removeAgentRow,
  });
  // ----- questions to the operator -----
  const questionMock = createQuestionMock({
    w,
    iso,
    ulid,
    reply,
    fail,
    emit,
    agent,
    audit: (action, target, proj, detail) => audit(action, target, proj, detail),
    sendMail: (to, from, subject, body, thread) => sendMail(to, from, subject, body, thread),
    questionClosed: (id, result) => waitMock.questionClosed(id, result),
    inboxEvent: (d) => inboxEvent(d),
    threadQuestion: (q, change) => threadMock.onQuestion(q, change),
  });
  // ----- Inbox threads (docs/api.md §6.13) -----
  const threadMock = createThreadMock({
    w,
    iso,
    ulid,
    reply,
    fail,
    emit,
    agent,
    audit: (action, target, proj, detail) => audit(action, target, proj, detail),
    inboxEvent: (d) => inboxEvent(d),
    retractQuestion: (id, reason, byOwner) => void questionMock.retractNow(id, reason, byOwner),
  });
  // ----- wakes: work for an agent that does not run -----
  /** The turns that wait for a stopped or paused agent. The rest of `queued` (mail, cards) has no body in the mock. */
  const queuedTurns: Record<string, QueuedTurn[]> = {};
  let nextQueued = 0;
  /** The agent is ready: the queued work goes to it. */
  const flushQueue = (a: Agent) => {
    const turns = queuedTurns[a.id] ?? [];
    delete queuedTurns[a.id];
    delete a.queued;
    for (const t of turns) deliverTurn(a, t.body);
  };
  /** Start a stopped agent (with resume) for `cause`, then deliver its queued work. A paused, live, or errored agent does not wake. */
  const wake = (a: Agent, cause: string) => {
    if (a.paused || a.status === "error" || LIVE.includes(a.status) || !w.agents.includes(a)) return;
    // A new session that waits for the next wake: the wake starts it.
    const fresh = a.reset_pending?.kind === "new_session" ? takeReset(a) : null;
    agentEvent(a, "agent.woken", `${a.name}: woken (${cause})`, { cause });
    openSession(a);
    setStatus(a, "starting", fresh ? `new session: requested by ${fresh.by}` : "woken");
    later(() => {
      setStatus(a, waitMock.restStatus(a));
      if (fresh) resetTurns(a, fresh);
      flushQueue(a);
    });
  };
  /**
   * One turn to a live agent. The daemon queues the turn until the agent is
   * idle; the fake harness then writes the user block, thinks, calls a tool,
   * and streams a reply. `system` is a daemon turn (`[System:<kind>] ...`)
   * in place of an operator turn.
   */
  /** The turn of each agent: an interrupt bumps it, and the steps of the old turn stop. */
  const turnGen: Record<string, number> = {};
  const deliverTurn = (a: Agent, text: string, system?: { kind: string; detail?: string }) => {
    const s = currentSession(a);
    const model = mockModel(a);
    const r = fakeReply(text, model);
    const pace = Math.max(step, latency > 0 ? 450 : 0);
    const typeTurn = () => {
      if (!LIVE.includes(a.status) || currentSession(a) !== s) return;
      if (a.status !== "idle" && a.status !== "waiting") return void later(typeTurn, Math.max(pace, 1));
      if (s) s.turns++;
      if (system) {
        writeBlock(a, systemTurn(system.kind, text, system.detail));
        agentEvent(a, "turn_start", `Turn typed by the daemon: ${text.slice(0, 60)}`, { text });
      } else {
        writeBlock(a, { role: "user", kind: "text", text: `[User] ${text}`, origin: "operator", parts: [{ source: "user", body: text }] });
        agentEvent(a, "turn_start", `Turn typed by operator: ${text.slice(0, 60)}`, { text }, "operator");
      }
      setStatus(a, "working");
      const gen = (turnGen[a.id] = (turnGen[a.id] ?? 0) + 1);
      const steps: (() => void)[] = [
        () => void writeBlock(a, r.thinking),
        () => void writeBlock(a, r.call),
        () => void writeBlock(a, r.result),
        () => {
          const b = writeBlock(a, r.reply);
          if (b && s) later(() => growBlock(s.id, b.seq, r.full), pace);
        },
        () => {
          agentEvent(a, "turn_end", "Turn ended");
          setStatus(a, waitMock.restStatus(a));
        },
      ];
      const run = (i: number) => {
        if (i >= steps.length || currentSession(a) !== s || turnGen[a.id] !== gen) return;
        steps[i]!();
        later(() => run(i + 1), i === 3 ? pace * 2 : pace);
      };
      later(() => run(0), pace);
    };
    later(typeTurn, 0);
  };
  // ----- resets by hand: a compaction or a new session (docs/api.md §6.5) -----
  // One reset at most waits for each agent (`reset_pending`); a new request
  // replaces it. It runs when the agent comes to rest and is not paused.
  type Reset = ResetPending & { then?: string };
  /** The "then" prompt of the reset that waits, by agent. */
  const resetThen: Record<string, string> = {};
  const atRest = (a: Agent) => a.status === "idle" || a.status === "waiting";
  /** Remove the reset that waits, and give it with its "then" prompt. */
  const takeReset = (a: Agent): Reset | null => {
    const p = a.reset_pending;
    const then = resetThen[a.id];
    delete a.reset_pending;
    delete resetThen[a.id];
    return p ? { ...p, ...(then !== undefined ? { then } : {}) } : null;
  };
  /** The turns after a reset: the recap (new session only), then the "then" prompt. */
  const resetTurns = (a: Agent, r: Reset) => {
    if (r.kind === "new_session" && r.recap) deliverTurn(a, "Your last session ended. A recap: you worked on the cards of your project. Read your cards and your mail to continue.", { kind: "recap" });
    if (r.then) deliverTurn(a, r.then, { kind: "reset", detail: `requested by ${r.by}` });
  };
  const runCompact = (a: Agent, r: Reset) => {
    const began = now();
    setStatus(a, "working", "compacting");
    later(() => {
      if (a.status !== "working" || a.status_reason !== "compacting") return;
      a.last_compact_at = iso();
      a.last_compact_ok = true;
      agentEvent(a, "agent.compacted", `${a.name}: compacted (manual)`, { cause: "manual", ok: true, duration_ms: now() - began });
      setStatus(a, waitMock.restStatus(a), "compacted");
      resetTurns(a, r);
      flushQueue(a);
    }, 1500);
  };
  /** End the current session (`new_session`) and start a new, clean one. */
  const runNewSession = (a: Agent, r: Reset) => {
    endSession(a, "new_session");
    openSession(a);
    setStatus(a, "starting", `new session: requested by ${r.by}`);
    later(() => {
      setStatus(a, waitMock.restStatus(a));
      resetTurns(a, r);
      flushQueue(a);
    });
  };
  onRest = (a: Agent) => {
    if (!a.reset_pending || a.paused) return;
    later(() => {
      if (!a.reset_pending || a.paused || !atRest(a) || !w.agents.includes(a)) return;
      const r = takeReset(a)!;
      if (r.kind === "compact") runCompact(a, r);
      else runNewSession(a, r);
    }, 0);
  };
  /** The "then" prompt of a reset request: absent, or not blank and at most 32 KiB. */
  const resetThenOf = (then: unknown): string | undefined => {
    if (then === undefined || then === null) return undefined;
    if (typeof then !== "string" || !then.trim()) fail(400, "bad_request", "`then` is blank; give a prompt, or leave it out");
    if (new TextEncoder().encode(then).length > 32 * 1024) fail(400, "bad_request", "`then` is longer than 32 KiB");
    return then;
  };
  /** Record the request: `reset_pending` for a reset that waits, `agent.reset`, and the 202 answer. */
  const acceptReset = (a: Agent, r: Reset, state: ResetState): ResetResult => {
    const replaced = !!a.reset_pending;
    if (replaced) takeReset(a);
    if (state !== "started") {
      const { then, ...pending } = r;
      a.reset_pending = pending;
      if (then !== undefined) resetThen[a.id] = then;
    }
    a.updated_at = iso();
    const what = r.kind === "compact" ? "compaction" : "new session";
    agentEvent(a, "agent.reset", `${a.name}: ${what} ${state === "started" ? "started" : state === "queued" ? "queued" : "at the next wake"} (by ${r.by})`, { kind: r.kind, by: r.by, then_set: r.then_set, deferred: r.deferred, replaced, state }, r.by);
    return { ...clone(a), reset: { kind: r.kind, state, replaced, deferred: r.deferred, then_set: r.then_set } };
  };
  const openSession = (a: Agent) => {
    // A start runs the active harness version.
    const v = harnessInstalled[a.harness];
    if (v) harnessRuns[a.id] = v;
    for (const s of w.sessions.filter((x) => x.agent === a.id && x.current)) {
      s.current = false;
      s.ended_at = iso();
      s.outcome = "restarted";
      frame(s.id, { type: "status", status: "ended" });
    }
    // A new session gets the skills of the list now.
    delete a.skills_restart;
    const harness = `fake-${ulid("hs")}`;
    const fresh: Session = { id: ulid("ses"), agent: a.id, harness_session_id: harness, started_at: iso(), turns: 0, current: true, resumes: 0 };
    w.sessions.unshift(fresh);
    a.session = harness;
    w.transcripts[fresh.id] = [];
    appendBlock(w.transcripts[fresh.id]!, fresh, a.harness, { role: "system", kind: "system", text: "Session started." }, iso());
    agentEvent(a, "session_started", `${a.name}: session started`, { harness_session: harness });
  };
  const endSession = (a: Agent, outcome: string) => {
    for (const s of w.sessions.filter((x) => x.agent === a.id && x.current)) {
      s.current = false;
      s.ended_at = iso();
      s.outcome = outcome;
      frame(s.id, { type: "status", status: "ended" });
    }
    delete a.session;
    delete a.skills_restart;
  };

  const eventsAfter = (q: EventQuery, base: (e: AgentEvent) => boolean) => {
    const since = typeof q.since === "string" && !/^\d+$/.test(q.since) ? (w.events.find((e) => e.id === q.since)?.seq ?? fail(400, "bad_request", `no event \`${q.since}\` for since`)) : Number(q.since ?? 0);
    const rows = (after: number) => w.events.filter((e) => e.seq > after && base(e) && (!q.kind || e.kind === q.kind) && (!q.project || e.project === q.project));
    // `order=desc`: newest first; `since` stays the lower bound and the cursor is the upper bound.
    if (q.order === "desc") {
      const before = q.cursor ? Number(q.cursor) : Infinity;
      return paginate(rows(since).filter((e) => e.seq < before).reverse().map(eventLink), { limit: q.limit }, (e) => String(e.seq));
    }
    return paginate(rows(q.cursor ? Number(q.cursor) : since).map(eventLink), { limit: q.limit }, (e) => String(e.seq));
  };

  /** A fake WebSocket to `/api/events`: the backlog after `?since=`, then live frames. */
  class MockEventSocket implements RawSocket {
    private handlers: Record<string, ((ev: SocketEvent) => void)[]> = {};
    private off: (() => void) | null = null;
    readyState = 0;
    binaryType: BinaryType = "blob";
    readonly protocol: string;
    /** `protocol` is what the "daemon" echoes (./ws); null refuses the upgrade. */
    constructor(url: string, protocol: string | null = "") {
      this.protocol = protocol ?? "";
      const params = new URL(url, "http://mock").searchParams;
      const since = Number(params.get("since") ?? 0);
      const withAudit = ["1", "true"].includes(params.get("audit") ?? "");
      queueMicrotask(() => {
        if (this.readyState !== 0) return;
        if (protocol === null) {
          this.readyState = 3;
          this.fire("error", {});
          this.fire("close", { code: 1006 });
          return;
        }
        this.readyState = 1;
        this.fire("open", {});
        let last = since;
        for (const e of w.events.filter((x) => x.seq > since)) {
          this.fire("message", { data: JSON.stringify(eventLink(e)) });
          last = e.seq;
        }
        const fn = (e: AgentEvent) => {
          if (e.seq > last) {
            last = e.seq;
            this.fire("message", { data: JSON.stringify(eventLink(e)) });
          }
        };
        hub.add(fn);
        // `audit` frames: live only, no `seq`, no backlog.
        const onAudit = (a: AuditEntry) => this.fire("message", { data: JSON.stringify({ kind: "audit", audit: auditLink(a) }) });
        if (withAudit) auditHub.add(onAudit);
        this.off = () => (hub.delete(fn), auditHub.delete(onAudit));
      });
    }
    addEventListener(type: string, fn: (ev: SocketEvent) => void) {
      (this.handlers[type] ??= []).push(fn);
    }
    removeEventListener(type: string, fn: (ev: SocketEvent) => void) {
      this.handlers[type] = (this.handlers[type] ?? []).filter((f) => f !== fn);
    }
    send() {}
    close() {
      if (this.readyState === 3) return;
      this.readyState = 3;
      this.off?.();
      this.fire("close", { code: 1000 });
    }
    private fire(type: string, ev: SocketEvent) {
      for (const fn of this.handlers[type] ?? []) fn(ev);
    }
  }

  // ----- fake activity -----
  const TOOLS = ["Read", "Edit", "Bash", "Grep", "Write"];
  const FILES = ["crates/terrariumd/src/runner.rs", "crates/terrarium-core/src/event.rs", "dashboard/src/api/client.ts", "Cargo.toml"];
  let tick = 0;
  const activity = () => {
    monitorMock.tick();
    pipelineMock.tick();
    tick++;
    const working = w.agents.filter((a) => a.status === "working");
    const a = working[tick % Math.max(working.length, 1)];
    if (a && tick % 5 !== 0) {
      const tool = TOOLS[tick % TOOLS.length]!;
      const target = tool === "Bash" ? "cargo test -p terrariumd" : FILES[tick % FILES.length]!;
      agentEvent(a, "tool_use", `${tool} ${target}`, { tool, input: target, tool_id: `toolu_mock${tick}` });
    }
    if (tick % 5 === 0) {
      const lead = w.agents.find((x) => x.name === "lead");
      if (lead?.status === "idle") {
        agentEvent(lead, "turn_start", "Turn started: triage inbox");
        setStatus(lead, "working");
      } else if (lead?.status === "working") {
        agentEvent(lead, "turn_end", "Turn ended after 12s");
        setStatus(lead, waitMock.restStatus(lead));
      }
    }
    if (tick % 7 === 0) {
      const reviewer = w.agents.find((x) => x.name === "reviewer");
      if (reviewer?.status === "starting") setStatus(reviewer, waitMock.restStatus(reviewer));
    }
    if (tick % 3 === 0) {
      for (const s of w.subscriptions) for (const win of s.windows) setWindow(s.name, win.window, Math.min(100, win.used_pct + 1));
    }
    // A plan at 100% resets after a while, so the demo shows the next period.
    for (const [name, at] of exhaustedAt) if (tick - at >= 6) resetPlan(name);
    if (tick % 2 === 0) usageTick();
    // Live audit rows for the Audit tab: an agent reads a secret, or the git credential helper answers.
    if (tick % 4 === 2 && a) {
      if (tick % 8 === 2) audit("secret.use", "GITHUB_TOKEN", a.project, { scope: a.project, via: "env" }, `agent:${a.id}`);
      else audit("repo.credential", `${a.project}/main`, a.project, { host: "github.com", ok: true }, `agent:${a.id}`);
    }
  };
  let timer: ReturnType<typeof setInterval> | undefined;
  const ensureActivity = () => {
    const ms = opts.eventsIntervalMs ?? 3000;
    if (!timer && ms > 0) timer = setInterval(activity, ms);
  };

  // ----- board (docs/api.md §8) -----
  const blockedBy = (c: Card) => c.depends_on.filter((d) => {
    const dep = w.cards.find((x) => x.id === d);
    return dep && dep.column !== "done" && !dep.archived;
  });
  // The session of a comment renders at read time, like the daemon: `removed` follows the agent.
  const view = (c: Card): Card => ({
    ...c,
    ...(testRunMock.cardTests(c.id) ? { tests: testRunMock.cardTests(c.id) } : {}),
    blocked_by: blockedBy(c),
    comments: c.comments.map((cm) => ({ ...cm, session: cm.session ? (sessionRef(cm.session.id) ?? null) : null })),
  });
  /** The distinct sessions of a card's comments and card events (`data.actor_session`), oldest first. */
  const cardSessions = (c: Card): SessionRef[] => {
    const ids = new Set<string>();
    for (const cm of c.comments) if (cm.session) ids.add(cm.session.id);
    for (const e of w.events) if (e.kind === "card" && e.card === c.id && typeof e.data.actor_session === "string") ids.add(e.data.actor_session);
    return [...ids]
      .map(sessionRef)
      .filter((s): s is SessionRef => !!s)
      .sort((a, b) => a.started_at.localeCompare(b.started_at) || a.id.localeCompare(b.id));
  };
  const activeIn = (proj: string, column: string) => w.cards.filter((c) => c.project === proj && c.column === column && !c.archived);
  /** The column of a `?column=` filter: a role alias names the role column; an unknown name matches no card. */
  const listColumn = (proj: string, name: string) => {
    const cols = w.columns[proj] ?? [];
    return cols.find((x) => x.name === name)?.name ?? ((name === "working" || name === "blocked") && cols.find((x) => x.role === name)?.name) ?? name;
  };
  const boardOf = (name: string) => {
    const p = project(name);
    const cols = w.columns[name] ?? [];
    const order = new Map(cols.map((c, i) => [c.name, i]));
    return {
      project: name,
      card_prefix: p.card_prefix,
      columns: cols.map((c, i) => ({ ...c, title: c.title || defaultTitle(c.name), position: i, count: activeIn(name, c.name).length })),
      cards: w.cards
        .filter((c) => c.project === name && !c.archived)
        .sort((a, b) => (order.get(a.column) ?? 99) - (order.get(b.column) ?? 99) || a.position - b.position)
        .map(view),
    };
  };
  const reindex = (proj: string, column: string) =>
    activeIn(proj, column)
      .sort((a, b) => a.position - b.position)
      .forEach((c, i) => (c.position = i));
  const cardEvent = (c: Card, change: string, summary: string, data: Record<string, unknown> = {}) => {
    const as = c.assignee ? w.agents.find((a) => a.id === c.assignee) : undefined;
    const column_role = roleOf(c.project, c.column) ?? null;
    emit({ kind: "card", actor: "operator", ...(as ? { agent: as.name, agent_id: as.id } : {}), project: c.project, card: c.id, summary, data: { card_id: c.id, change, title: c.title, column: c.column, column_role, ...data } });
  };
  /** The role of a column of the board of `proj`. */
  const roleOf = (proj: string, column: string): ColumnRole | undefined => (w.columns[proj] ?? []).find((x) => x.name === column)?.role;
  /** A column by name, or by a role alias (`working`, `blocked`): store/board.rs `column`. A name wins over an alias. */
  const columnOf = (proj: string, name: string): Column => {
    const cols = w.columns[proj] ?? [];
    const list = cols.map((x) => (x.role ? `${x.name} (${x.role})` : x.name)).join(", ");
    const byName = cols.find((x) => x.name === name);
    if (byName) return byName;
    if (name === "working" || name === "blocked")
      return cols.find((x) => x.role === name) ?? fail(422, "invalid", `the board of \`${proj}\` has no \`${name}\` column (no column has the role \`${name}\`); columns: ${list}`);
    return fail(422, "invalid", `no column \`${name}\`; columns: ${list}`);
  };
  const comment = (c: Card, body: string) => {
    const cm: CardComment = { id: ulid("cmt"), author: "operator", author_name: "operator", body, created_at: iso(), session: null };
    c.comments.push(cm);
    return cm;
  };
  /** Move with the rules of docs/api.md §8.2. */
  const move = (c: Card, to: string, position: number | undefined, note?: string) => {
    if (c.archived) fail(409, "conflict", `${c.id} is archived`);
    const col = columnOf(c.project, to);
    const column = col.name;
    const from = c.column;
    const fromRole = roleOf(c.project, from) ?? null;
    if (column !== from) {
      if (col.wip_limit !== undefined && activeIn(c.project, column).length >= col.wip_limit)
        fail(409, "wip_limit", `column \`${column}\` is at its WIP limit (${col.wip_limit})`, { column, wip_limit: col.wip_limit });
      if (col.role === "working") {
        const unmet = blockedBy(c);
        if (unmet.length) fail(409, "dependencies_unmet", `${c.id} depends on ${unmet.join(", ")} that are not done`, { blocked_by: unmet });
      }
      if (col.role === "blocked" && !note?.trim()) fail(422, "comment_required", `moving ${c.id} to \`${column}\` (the blocked column) needs a comment that says what is needed`);
    }
    if (note?.trim()) comment(c, note.trim());
    c.column = column;
    // `done_at` (docs/api.md §8): set on a move to done, cleared on a move out of it.
    if (from !== column) {
      if (column === "done") c.done_at = iso();
      else delete c.done_at;
    }
    const target = position ?? Number.MAX_SAFE_INTEGER;
    activeIn(c.project, column)
      .filter((x) => x !== c && x.position >= target)
      .forEach((x) => x.position++);
    c.position = target === Number.MAX_SAFE_INTEGER ? 1e9 : target;
    reindex(c.project, column);
    if (from !== column) reindex(c.project, from);
    c.updated_at = iso();
    if (from !== column) {
      cardEvent(c, "moved", `operator moved ${c.id} to ${column}`, { from, to: column, from_role: fromRole, to_role: col.role ?? null, comment: note ?? null });
      if (col.role === "blocked") {
        // The daemon's notice (store/board.rs notify_blocked): from `system` to the operator.
        sendMail("operator", "system", `${c.id} blocked: ${c.title}`, `Card ${c.id} "${c.title}" moved to ${column} (the blocked column) by operator.\n\nComment:\n${note ?? ""}\n\nRun \`terrarium card show ${c.id}\`.`);
      }
      if (column === "done") {
        for (const d of w.cards.filter((x) => x.depends_on.includes(c.id) && blockedBy(x).length === 0)) cardEvent(d, "unblocked", `${d.id} unblocked by ${c.id}`, { by: c.id });
      }
    } else cardEvent(c, "reordered", `operator reordered ${c.id}`);
  };
  const assign = (c: Card, assignee: string | null) => {
    const previous = c.assignee ?? null;
    if (assignee === null || assignee === "") {
      delete c.assignee;
      delete c.assignee_name;
    } else if (assignee === "operator" || assignee === "me") {
      c.assignee = "operator";
      c.assignee_name = "operator";
    } else {
      const local = w.agents.find((a) => a.project === c.project && (a.name === assignee || a.id === assignee || `${a.project}/${a.name}` === assignee));
      const a = local ?? fail(422, "invalid", `\`${assignee}\` is not an agent of project \`${c.project}\``);
      c.assignee = a.id;
      c.assignee_name = `${a.project}/${a.name}`;
    }
    c.updated_at = iso();
    cardEvent(c, "assigned", `operator assigned ${c.id} to ${c.assignee_name ?? "nobody"}`, { assignee: c.assignee ?? null, previous });
  };
  /** The ETA (store/board.rs set_eta): set from a duration or a time, or clear with `null`. A change to another's card is audited. */
  const setEta = (c: Card, v: string | null) => {
    if (c.archived) fail(409, "conflict", `${c.id} is archived`);
    const previous = c.eta_at ?? null;
    if (v === null) {
      if (!previous) return;
      for (const k of ["eta_at", "eta_set_at", "eta_set_by", "eta_set_by_name"] as const) delete c[k];
    } else {
      if (c.column === "done") fail(409, "conflict", `${c.id} is done; a done card takes no new ETA`);
      const r = resolveEta(v);
      if ("error" in r) fail(422, "invalid", r.error);
      c.eta_at = new Date(r.at).toISOString();
      c.eta_set_at = iso();
      c.eta_set_by = "operator";
      c.eta_set_by_name = "operator";
    }
    c.updated_at = iso();
    const mine = c.assignee ? c.assignee === "operator" : c.created_by === "operator";
    if (!mine) audit("card.eta", c.id, c.project, { eta_at: c.eta_at ?? null, previous });
    cardEvent(c, "eta", c.eta_at ? `${c.id} ETA ${c.eta_at} by operator` : `${c.id} ETA cleared by operator`, { eta_at: c.eta_at ?? null, previous });
  };

  // ----- mail (docs/api.md §7) -----
  /** The loop guard limit (`TERRARIUM_MAIL_LOOP_LIMIT`, default 6). */
  const LOOP_LIMIT = 6;
  const byId = (a: Message, b: Message) => a.id.localeCompare(b.id);
  /** Agent-to-agent messages on the thread since its last operator or project-inbox message, or its last resume. */
  const agentRun = (thread: string): number => {
    let n = 0;
    const reset = w.mailResets?.[thread] ?? "";
    for (const m of w.mail.filter((x) => x.thread_id === thread).sort(byId)) {
      if (m.id <= reset || m.from_kind === "operator" || m.from_kind === "project" || m.to_kind === "project") n = 0;
      else if (m.from_kind === "agent" && m.to_kind === "agent") n++;
    }
    return n;
  };
  /** The row as the daemon renders it: `thread_paused` only when true. */
  const render = (m: Message): Message => mailLink(agentRun(m.thread_id) > LOOP_LIMIT ? { ...m, thread_paused: true } : m);
  const sendMail = (to: string, from: string, subject: string, body: string, thread?: string, inReplyTo?: string): Message => {
    if (to === "system") fail(422, "invalid", "`system` only sends mail");
    const t = address(to);
    const f = address(from);
    const threadId = thread ?? ulid("th");
    const held = f.kind === "agent" && t.kind === "agent" && agentRun(threadId) >= LOOP_LIMIT;
    const threadWas = inboxMock.threadOpen(threadId);
    // A watch of the thread closes with its agent's answer and opens again with the operator's next message.
    const watchWas = inboxMock.watchOpen(threadId);
    const m: Message = {
      id: ulid("msg"),
      thread_id: threadId,
      from: f.addr,
      from_kind: f.kind,
      from_name: f.label,
      to: t.addr,
      to_kind: t.kind,
      to_name: t.label,
      subject,
      body,
      state: t.kind === "agent" && !held ? "queued" : "delivered",
      ...(inReplyTo ? { in_reply_to: inReplyTo } : {}),
      created_at: iso(),
      ...(t.kind === "agent" && !held ? {} : { delivered_at: iso() }),
    };
    w.mail.push(m);
    if (held) (w.heldMail ??= []).push(m.id);
    mailEvent(m, held ? { held: true, thread_state: "paused" } : {});
    if (m.to_kind === "operator" && !isPauseNotice(m)) {
      const item = inboxMock.mailItem(m);
      inboxEvent({ item: item.id, kind: "mail", change: "opened", ...(item.project ? { project: item.project } : {}) });
    }
    inboxMock.threadEvent(threadId, threadWas);
    inboxMock.watchEvent(threadId, watchWas);
    if (held) {
      // One notice for each pause (store/mail.rs pause_notice).
      const reset = w.mail.filter((x) => x.thread_id === threadId && (x.from_kind === "operator" || x.from_kind === "project" || x.to_kind === "project")).sort(byId).at(-1)?.id ?? "";
      const sent = w.mail.some((x) => x.thread_id === threadId && x.from_kind === "system" && x.id > reset && /^Thread \S+ needs a human/.test(x.subject));
      if (!sent) {
        sendMail("operator", "system", `Thread ${threadId} needs a human`, `thread ${threadId} needs a human: ${agentRun(threadId)} agent replies without operator input. The daemon types no more mail pointers for this thread. Reply on the thread to resume it.`, threadId);
      }
    }
    return render(m);
  };
  /** A `mail` event about one message. */
  const mailEvent = (m: Message, extra: Record<string, unknown>) => {
    const recipient = m.to_kind === "agent" ? w.agents.find((a) => a.id === m.to) : undefined;
    emit({
      kind: "mail",
      actor: m.from_kind === "agent" ? `agent:${m.from}` : m.from_kind === "system" ? "system" : "operator",
      ...(recipient ? { agent: recipient.name, agent_id: recipient.id } : {}),
      project: m.to_kind === "project" ? m.to.slice(8) : recipient?.project,
      summary: `Mail from ${m.from_name} to ${m.to_name}: ${m.subject}`,
      data: { message_id: m.id, thread_id: m.thread_id, from: m.from, from_name: m.from_name, to: m.to, to_name: m.to_name, subject: m.subject, ...extra },
    });
  };
  const inboxEvent = (d: InboxEventData) =>
    emit({ kind: "inbox", actor: "system", ...(d.project ? { project: d.project } : {}), summary: `inbox ${d.change}: ${d.item}`, data: d as unknown as Record<string, unknown> });
  const inboxMock = createInboxMock({
    w,
    iso,
    reply,
    fail,
    loopLimit: LOOP_LIMIT,
    agentRun,
    render: (m) => render(m),
    replyTo: (id, body, subject, as) => replyTo(id, body, subject, as),
    sendMail: (to, from, subject, body, thread) => sendMail(to, from, subject, body, thread),
    mailEvent: (m, data) => mailEvent(m, data),
    inboxEvent: (d) => inboxEvent(d),
    agent: (key) => agent(key),
    audit: (action, target, proj, detail) => audit(action, target, proj, detail),
    threadItems: () => threadMock.items(),
  });
  /** The first read sets `read_by` (docs/api.md §7). */
  const readBy = (m: Message, reader: string) => {
    if (m.read_by) return;
    const r = address(reader);
    m.read_by = r.addr;
    m.read_by_name = r.label;
  };
  const replyTo = (id: string, body: string, subject: string | undefined, as?: string): Message[] => {
    const orig = find(w.mail, (m) => m.id === id, `message ${id}`);
    if (!body.trim()) fail(422, "invalid", "the body is empty");
    // The operator answers for `operator` and every project inbox.
    const from = as ?? (orig.to_kind === "agent" ? "operator" : orig.to);
    const participants = [...new Set(w.mail.filter((m) => m.thread_id === orig.thread_id).flatMap((m) => [m.from, m.to]))].filter((p) => p !== from && p !== "system");
    const first = w.mail.find((m) => m.thread_id === orig.thread_id);
    const subj = subject ?? (orig.subject || first?.subject ? `Re: ${(orig.subject || first!.subject).replace(/^Re:\s*/i, "")}` : "");
    const out = participants.map((p) => sendMail(p, from, subj, body, orig.thread_id, orig.id));
    orig.state = "replied";
    orig.read_at ??= iso();
    readBy(orig, "operator");
    return out.map(render);
  };
  /** Oldest first by ID; the cursor is a message ID and the next page is `id > cursor`, as in store/mail.rs. */
  const listMail = (pred: (m: Message) => boolean, q: { state?: string; thread?: string; unread?: boolean } & PageQuery): Page<Message> => {
    if (q.state && !["queued", "delivered", "read", "replied", "unread", "all"].includes(q.state)) fail(400, "bad_request", `bad state \`${q.state}\`: use queued, delivered, read, replied, or unread`);
    const limit = Math.max(1, Math.min(q.limit ?? 100, 1000));
    const rows = w.mail
      .filter(
        (m) =>
          pred(m) &&
          (!q.cursor || m.id > q.cursor) &&
          (!q.thread || m.thread_id === q.thread) &&
          (!q.unread || m.state === "queued" || m.state === "delivered") &&
          (!q.state || q.state === "all" || (q.state === "unread" ? m.state === "queued" || m.state === "delivered" : m.state === q.state)),
      )
      .sort(byId);
    const items = rows.slice(0, limit).map(render);
    return { items, next: rows.length > limit ? items.at(-1)!.id : null };
  };
  const markRead = (id: string) => {
    const m = find(w.mail, (x) => x.id === id, `message ${id}`);
    if (m.state === "queued" || m.state === "delivered") {
      m.state = "read";
      m.read_at = iso();
    }
    readBy(m, "operator");
    return render(m);
  };

  // ----- artifact upload -----
  const addArtifact = (proj: string, name: string, content_type: string, size: number, kind: Artifact["kind"], attach?: ArtifactAttach): Artifact => {
    project(proj);
    const n = Math.max(0, ...w.artifacts.filter((x) => x.project === proj).map((x) => Number(x.id.split("-")[1]) || 0)) + 1;
    const links: Artifact["links"] = attach ? [{ ...linkOf(attach.kind, attach.ref), created_by: "operator", created_at: iso() }] : [];
    const a: Artifact = {
      id: `ART-${n}`, key: `${proj}/ART-${n}`, uid: ulid("art"), project: proj, name, content_type, size, sha256: "0".repeat(64), kind,
      renders: kind === "bundle" || /^(image|text)\//.test(content_type) || content_type === "application/pdf",
      created_by: "operator", created_by_name: "operator", created_at: iso(), links, embedded_in: [],
    };
    w.artifacts.push(a);
    emit({ kind: "artifact.created", actor: "operator", project: proj, summary: `operator uploaded ${a.id} ${name}`, data: { artifact_id: a.id, key: a.key, uid: a.uid, name, kind, size, content_type } });
    return a;
  };

  // ----- subscriptions and the quota policy (docs/api.md §16) -----
  const PROBES = ["anthropic-oauth-usage", "openai-codex-usage", "codex-rate-limits", "github-copilot-quota", "copilot-premium-requests", "moonshot-balance", "moonshot-cn-balance"];
  const KEY_ENV: Record<string, string> = {
    anthropic: "ANTHROPIC_API_KEY",
    openai: "OPENAI_API_KEY",
    "openai-compatible": selfhosted.SELFHOSTED_KEY_ENV,
    selfhosted: selfhosted.SELFHOSTED_KEY_ENV,
    google: "GEMINI_API_KEY",
    gemini: "GEMINI_API_KEY",
    openrouter: "OPENROUTER_API_KEY",
    xai: "XAI_API_KEY",
    groq: "GROQ_API_KEY",
    github: "COPILOT_GITHUB_TOKEN",
    zai: ZAI_KEY_ENV,
    moonshot: MOONSHOT_KEY_ENV,
  };
  /** The subscription as the API shows it (docs/api.md §16.1). */
  const subView = (s: Subscription): Subscription => ({
    ...s,
    display_name: s.label ?? mockDisplayName(s),
    local_estimate_only: !s.probe,
    has_api_key: !!s.env_var && !!w.apiKeys[s.name],
    held_agents: w.holds.filter((h) => h.cause === `limit:${s.name}`).length,
    default_models: defaultModels(s),
    agents: w.agents.filter((a) => a.subscription === s.name).map((a) => ({ id: a.id, name: a.name, project: a.project })),
    default: !!s.default,
    enabled: s.enabled !== false,
    no_quota: selfhosted.isSelfHosted(s.provider),
    ...(s.selfhosted?.max_concurrent ? { streams: { max: s.selfhosted.max_concurrent, in_use: Math.min(s.streams?.in_use ?? 0, s.selfhosted.max_concurrent), waiting: s.streams?.waiting ?? 0 } } : { streams: undefined }),
    status_url: selfhosted.statusUrlOf(s.provider, s.selfhosted),
  });
  /** Enable or disable `s`: the audit row and the event, on a change. An enable wakes the agents with queued work. */
  const setSubEnabled = (s: Subscription, enabled: boolean): Subscription => {
    if ((s.enabled !== false) === enabled) return s;
    s.enabled = enabled;
    s.updated_at = iso();
    const kind = enabled ? "subscription.enabled" : "subscription.disabled";
    audit(kind, s.name, undefined, {});
    emit({ kind, actor: "operator", summary: `${s.name}: ${enabled ? "enabled" : "disabled"}`, data: { subscription: s.name, enabled } });
    return s;
  };
  /** PLAN §16.3: the harnesses with a default plan account. */
  const DEFAULT_HARNESSES = ["claude", "codex", "copilot"];
  /** The default of `s` goes away (its delete or logout). The daemon picks no new one. */
  const clearDefault = (s: Subscription) => {
    if (!s.default) return false;
    s.default = false;
    emit({ kind: "subscription", actor: "operator", summary: `${s.account?.harness ?? "the harness"} has no default account: ${s.name} is gone`, data: { change: "default_cleared", harness: s.account?.harness, from: s.name, to: null } });
    return true;
  };
  /** Make `s` the default of `harness`, with the audit row and the `subscription` event. */
  const setDefault = (s: Subscription, harness: string, reason: string, actor = "operator") => {
    const old = w.subscriptions.find((x) => x.default && x.account?.harness === harness);
    if (old) old.default = false;
    s.default = true;
    const detail = { harness, from: old?.name ?? null, to: s.name, reason };
    audit("subscription.default", s.name, undefined, detail, actor);
    emit({ kind: "subscription", actor, summary: `${s.name} is the default ${harness} account`, data: { change: "default", ...detail } });
  };
  /** PLAN §16.3: a pool holds accounts of one provider, and the harness can spend each. The current subscription goes first. */
  /** PLAN §16.3: pi uses API keys only. A plan login of a provider that sells one, unless its plan says "API key". */
  const PLAN_PROVIDERS = ["anthropic", "claude", "openai", "openai-codex", "codex", "chatgpt", "github", "github-copilot", "copilot", "google-gemini-cli"];
  const piPlan = (s: Subscription) => PLAN_PROVIDERS.includes(s.provider.trim().toLowerCase()) && (s.plan ?? "").trim().toLowerCase() !== "api key";
  /** A pi agent, its pool, or a pi template names no plan login. */
  const checkPiPlans = (harness: Agent["harness"], names: (string | undefined)[]) => {
    for (const n of names) {
      const s = n ? w.subscriptions.find((x) => x.name === n) : undefined;
      // A self-hosted server serves the harnesses of its API style only.
      const why = s ? selfhosted.harnessRefusal(s, harness) : null;
      if (why) fail(422, "invalid", why);
      if (harness === "pi" && s && piPlan(s)) fail(422, "invalid", `subscription \`${s.name}\`: ${PI_PLAN_REFUSAL}`);
    }
  };
  /** Why no agent may start on the subscription of `a`: it is disabled. */
  const disabledRefusal = (name: string | undefined): string | null => {
    const s = name ? w.subscriptions.find((x) => x.name === name) : undefined;
    return s && s.enabled === false ? `subscription \`${s.name}\` is disabled; enable it first (terrarium admin subscription enable ${s.name})` : null;
  };
  const checkPool = (harness: Agent["harness"], current: string | undefined, pool: string[]): string[] => {
    const names = [...new Set(pool.map((n) => n.trim()).filter(Boolean))];
    checkPiPlans(harness, [current, ...names]);
    if (!names.length) return [];
    if (current && !names.includes(current)) names.unshift(current);
    let provider: string | undefined;
    for (const n of names) {
      const s = w.subscriptions.find((x) => x.name === n);
      if (!s) fail(422, "invalid", `\`subscription_pool\` names \`${n}\`, but there is no such subscription`);
      if (!s!.harnesses.includes(harness)) fail(422, "invalid", `the ${harness} harness cannot spend subscription \`${n}\` of the pool`);
      if (provider && provider !== s!.provider) fail(422, "invalid", `the pool mixes providers: \`${n}\` is ${s!.provider}, the others are ${provider}; a pool holds accounts of one provider`);
      provider ??= s!.provider;
    }
    return names;
  };
  /** Logout and delete wait until no live agent runs on the account. */
  const refuseLiveAccount = (s: Subscription, verb: string) => {
    if (!s.account) return;
    const live = w.agents.filter((a) => a.subscription === s.name && LIVE.includes(a.status));
    if (live.length) fail(409, "conflict", `${live.length} running agent(s) use account \`${s.name}\` (${live.map((a) => `${a.project}/${a.name}`).join(", ")}); move them to another account or stop them before you ${verb}`);
  };
  const checkSubName = (name: string) => {
    if (!NAME_RE.test(name)) fail(422, "invalid", `\`${name}\` is not a valid name: lowercase letters, digits, - and _`);
    if (name === "login" || name === "detect") fail(422, "invalid", `\`${name}\` is a reserved name`);
  };
  /** The lines that notified, `<sub>/<window>/<line>` -> the reset of that period (quota_notices.rs). */
  const notified = new Map<string, string | undefined>();
  const exhaustedAt = new Map<string, number>();
  /** The quota lines (docs/api.md §16.6): `alert_at`, 90 when `alert_at` is under 90, and 100 (used up). */
  const quotaLines = (alertAt: number): number[] => [...(alertAt >= 1 && alertAt < 100 ? [alertAt] : []), ...(alertAt < 90 ? [90] : []), 100];
  // The seed: the lines that the windows passed already notified (the seed mail of chatgpt-pro).
  for (const s of w.subscriptions) for (const win of s.windows) for (const line of quotaLines(s.alert_at)) if (win.used_pct >= line) notified.set(`${s.name}/${win.window}/${line}`, win.resets_at);
  const fewNames = (names: string[]) => `${names.slice(0, 5).join(", ")}${names.length > 5 ? `, and ${names.length - 5} more` : ""}`;
  /** One mail to the operator for the highest line that the window passed in this period (quota_notices.rs). */
  const lineNotice = (s: Subscription, win: Subscription["windows"][number]) => {
    const pct = win.used_pct;
    let passed: number | undefined;
    for (const line of quotaLines(s.alert_at)) {
      const key = `${s.name}/${win.window}/${line}`;
      if (notified.has(key)) {
        const stored = notified.get(key);
        const storedAt = stored ? Date.parse(stored) : NaN;
        const sampleAt = win.resets_at ? Date.parse(win.resets_at) : NaN;
        const newPeriod = storedAt <= now() && (Number.isNaN(sampleAt) || sampleAt > storedAt + 5 * 60_000);
        if (pct <= line - 10 || newPeriod) notified.delete(key);
      }
      if (pct >= line && !notified.has(key)) {
        notified.set(key, win.resets_at);
        passed = line;
      }
    }
    if (passed === undefined) return;
    const usedUp = passed >= 100;
    const resets = win.resets_at ? `The window resets in ${Math.max(1, Math.round((Date.parse(win.resets_at) - now()) / 60_000))}m (${win.resets_at}).` : "The provider gives no reset time for this window.";
    let subject: string;
    let body: string;
    const on = w.agents.filter((a) => a.subscription === s.name).map((a) => `${a.project}/${a.name}`);
    const agentsLine = on.length ? `The agents on this subscription: ${fewNames(on)}.` : "No agent is on this subscription now.";
    if (usedUp) {
      subject = `Quota: ${s.name} ${win.window} window is used up`;
      body = `Subscription \`${s.name}\` (${s.provider}) has used all of its ${win.window} window, as the probe reads it.\n${resets}\n${agentsLine}\n\nThe agents keep their turns. An agent stops only when the provider refuses a turn: then it moves to the next account of its pool, or it waits, and you get a mail. To move agents now, use \`terrarium admin agent swap <name> <subscription>\`.\n\nOpen: /subscriptions\n`;
    } else {
      const next = quotaLines(s.alert_at).find((l) => l > passed! && l < 100);
      subject = `Quota: ${s.name} ${win.window} window at ${pct}%`;
      body = `Subscription \`${s.name}\` (${s.provider}) has used ${pct}% of its ${win.window} window. It passed the ${passed}% line.\n${resets}\n${agentsLine}\n${next ? `You get one more message at ${next}%.\n` : ""}\nMove agents to another account with \`terrarium admin agent swap <name> <subscription>\`, give them an account pool for the auto-swap, or wait for the reset.\n\nOpen: /subscriptions\n`;
    }
    const m = sendMail("operator", "system", subject, body);
    emit({ kind: "quota.alert", actor: "system", summary: subject, data: { subscription: s.name, window: win.window, used_pct: pct, resets_at: win.resets_at ?? null, level: usedUp ? "exhausted" : "alert", alert_at: s.alert_at, threshold: passed, message: m.id } });
  };
  /** The quota policy (docs/api.md §16.6) after a new reading of one window: the line mail only. A probe window holds no turn. */
  const policy = (s: Subscription, win: Subscription["windows"][number]) => {
    if (win.used_pct >= 100) exhaustedAt.set(s.name, tick);
    else if (s.windows.every((x) => x.used_pct < 100)) exhaustedAt.delete(s.name);
    lineNotice(s, win);
  };
  /** A probe reading: store the samples, emit `quota.sample`, and run the policy. */
  const reading = (s: Subscription) => {
    const at = iso();
    for (const win of s.windows) {
      win.at = at;
      w.quota.push({ subscription: s.name, window: win.window, used_pct: win.used_pct, resets_at: win.resets_at, raw: { utilization: win.used_pct, resets_at: win.resets_at }, at });
    }
    s.probe_status = "ok";
    delete s.probe_error;
    s.last_probe_at = at;
    s.last_success_at = at;
    emit({ kind: "quota.sample", actor: "system", summary: `${s.name}: ${s.windows.map((x) => `${x.window} ${x.used_pct}%`).join(", ")}`, data: { subscription: s.name, windows: s.windows.map((x) => ({ window: x.window, used_pct: x.used_pct, resets_at: x.resets_at ?? null })) } });
    for (const win of s.windows) policy(s, win);
  };
  const setWindow = (name: string, window: string, usedPct: number) => {
    const s = sub(name);
    const win = s.windows.find((x) => x.window === window);
    if (!win) s.windows.push({ window, used_pct: usedPct, resets_at: new Date(now() + 5 * 3600_000).toISOString(), at: iso() });
    else win.used_pct = usedPct;
    reading(s);
  };
  const resetPlan = (name: string) => {
    const s = sub(name);
    for (const win of s.windows.filter((x) => x.used_pct >= 100)) {
      win.used_pct = 4;
      win.resets_at = new Date(now() + 5 * 3600_000).toISOString();
    }
    reading(s);
  };
  /** Fake local accounting while agents work: one `usage.sample` per working agent. */
  const usageTick = () => {
    for (const a of w.agents.filter((x) => x.status === "working" && x.subscription)) {
      const card = currentCard(a.id);
      const row: UsageRow = { at: iso(), agent: a.id, project: a.project, subscription: a.subscription!, model: a.model ?? "claude-opus-5", input_tokens: 800 + (tick % 5) * 120, output_tokens: 140 + (tick % 3) * 40, cache_read: 21000, cache_write: 900, messages: 2, turn: w.seq, ...(card ? { card } : {}) };
      w.usage.push(row);
      agentEvent(a, "usage.sample", `${a.name}: ${row.input_tokens + row.output_tokens} tokens on ${row.subscription}`, {
        subscription: row.subscription, harness: a.harness, model: row.model, input_tokens: row.input_tokens, output_tokens: row.output_tokens, cache_read: row.cache_read, cache_write: row.cache_write, messages: row.messages,
      });
    }
  };

  /** The logins in the harness homes (what the status commands would find). */
  const homeLogins = () => [
    ...w.subscriptions.filter((s) => s.login_status === "logged_in" || s.login_status === "expiring" ).map((s) => ({ provider: s.provider, harness: s.harnesses[0] ?? "claude" })),
    // A Codex login in the shared home with no subscription. pi uses API keys only: no pi login.
    { provider: "openai", harness: "codex" as const },
  ];
  const detected = () =>
    homeLogins()
      .filter((l) => !w.subscriptions.some((s) => s.provider === l.provider && s.harnesses.includes(l.harness)))
      .map((l) => ({
        kind: l.harness,
        harness: l.harness,
        provider: l.provider,
        logged_in: true,
        detail: `${l.harness}: logged in`,
        suggested: { name: `${l.provider}-${l.harness}`, provider: l.provider, harnesses: [l.harness], alert_at: 80 },
      }));

  // ----- local accounting (docs/api.md §16.4 `GET /api/usage`) -----
  /** The attribution rule of docs/api.md §16.1: the agent's card in the working column, newest `updated_at` first. */
  const currentCard = (agentId: string): string | undefined =>
    w.cards
      .filter((c) => c.assignee === agentId && roleOf(c.project, c.column) === "working" && !c.archived)
      .sort((x, y) => y.updated_at.localeCompare(x.updated_at) || x.id.localeCompare(y.id))[0]?.id;
  const cardTitle = (id: string) => w.cards.find((c) => c.id === id)?.title;
  const sumRows = (rows: UsageRow[]): UsageTotals => {
    const t: UsageTotals = { input_tokens: 0, output_tokens: 0, cache_read: 0, cache_write: 0, messages: 0, turns: 0 };
    const turns = new Set<string>();
    for (const r of rows) {
      t.input_tokens += r.input_tokens;
      t.output_tokens += r.output_tokens;
      t.cache_read += r.cache_read;
      t.cache_write += r.cache_write;
      t.messages += r.messages;
      turns.add(`${r.agent}/${r.turn}`);
    }
    t.turns = turns.size;
    return t;
  };
  const checkTime = (what: string, v?: string) => {
    if (v && Number.isNaN(Date.parse(v))) fail(400, "bad_request", `\`${what}\` is not an RFC 3339 time`);
    return v ? new Date(v).toISOString() : undefined;
  };
  const usageSeries = (q: UsageQuery): UsagePoint[] => {
    const since = checkTime("since", q.since) ?? new Date(now() - 24 * 3600_000).toISOString();
    const until = checkTime("until", q.until);
    const agentId = q.agent ? agent(q.agent).id : undefined;
    const card = q.card?.trim().toUpperCase();
    const rows = w.usage.filter(
      (r) =>
        r.at >= since &&
        (!until || r.at < until) &&
        (!q.subscription || r.subscription === q.subscription) &&
        (!q.project || r.project === q.project) &&
        (!agentId || r.agent === agentId) &&
        (!q.model || r.model === q.model) &&
        (!card || r.card === card),
    );
    if (q.group && !["subscription", "project", "agent", "model", "card"].includes(q.group)) fail(400, "bad_request", `unknown group \`${q.group}\``);
    const bucketOf = (at: string) => (q.bucket === "day" ? `${at.slice(0, 10)}T00:00:00Z` : `${at.slice(0, 13)}:00:00Z`);
    const groups = new Map<string, UsageRow[]>();
    for (const r of rows) {
      const k = `${bucketOf(r.at)}\u0000${q.group ? (r[q.group] ?? "") : ""}`;
      groups.set(k, [...(groups.get(k) ?? []), r]);
    }
    return [...groups.entries()]
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
      .map(([k, rs]) => {
        const [bucket, key] = k.split("\u0000") as [string, string];
        // A sample with no card has no key (the daemon's null).
        const title = q.group === "card" && key ? cardTitle(key) : undefined;
        return { bucket, ...(q.group && key ? { key } : {}), ...(title ? { title } : {}), ...sumRows(rs) };
      });
  };
  const execSessions = new Map<string, string>();
  const target = (id: string): Target => find(w.targets, (t) => t.id === id || t.name === id, `target \`${id}\``);

  // ----- target leases (docs/api.md §17.7) -----
  w.leases ??= [];
  const leaseEvent = (l: Lease, change: "granted" | "extended" | "ended", actor: string) => {
    const a = w.agents.find((x) => x.id === l.agent);
    const holder = l.agent_name ?? l.agent;
    const summary = change === "ended" ? `lease on ${l.target} ended for ${holder}` : `${holder} ${change} target ${l.target} ${l.expires_at ? `until ${l.expires_at}` : "until revoked"}`;
    emit({ kind: "lease", actor, ...(a ? { agent: a.name } : {}), agent_id: l.agent, project: l.project, summary, data: { change, target: l.target, lease_id: l.id, expires_at: l.expires_at, exclusive: l.exclusive, state: l.state } });
  };
  /** The expiry timer: end each active lease past its `expires_at`, as `system`. */
  const sweepLeases = () => {
    for (const l of w.leases) {
      if (l.state !== "active" || !l.expires_at || Date.parse(l.expires_at) > now()) continue;
      l.state = "expired";
      l.revoked_at = iso();
      l.revoked_by = "system";
      audit("target.lease_expired", l.target, l.project, { lease: l.id, agent: l.agent, agent_name: l.agent_name }, "system");
      leaseEvent(l, "ended", "system");
    }
  };
  const armExpiry = (l: Lease) => {
    if (!l.expires_at) return;
    const ms = Date.parse(l.expires_at) - now();
    if (ms < 2 ** 31 - 1) later(sweepLeases, Math.max(0, ms) + 20);
  };
  w.leases.forEach(armExpiry);
  /** `until`: an RFC 3339 time in the future, or a duration from now (`90s`, `30m`, `2h`, `1d`). */
  const parseUntil = (u: string | null | undefined): string | null => {
    const v = u?.trim();
    if (!v) return null;
    const d = /^(\d+)\s*(s|m|h|d)$/.exec(v);
    const UNIT = { s: 1000, m: 60_000, h: 3_600_000, d: 86_400_000 } as const;
    const at = d ? now() + Number(d[1]) * UNIT[d[2] as keyof typeof UNIT] : /^\d{4}-\d{2}-\d{2}T/.test(v) ? Date.parse(v) : NaN;
    if (Number.isNaN(at)) fail(422, "invalid", `bad \`until\` \`${v}\`: an RFC 3339 time or a duration such as 30m`);
    if (at <= now()) fail(422, "invalid", `\`until\` \`${v}\` is not in the future`);
    return new Date(at).toISOString();
  };
  const leasesWhere = (pred: (l: Lease) => boolean, all?: boolean) => {
    sweepLeases();
    const rows = w.leases.filter(pred).reverse();
    return all ? rows.filter((l) => l.state === "active").concat(rows.filter((l) => l.state !== "active").slice(0, 100)) : rows.filter((l) => l.state === "active");
  };
  const activeLease = (t: Target, a: Agent): Lease => {
    sweepLeases();
    return w.leases.find((l) => l.target === t.name && l.agent === a.id && l.state === "active") ?? notFound(`active lease of \`${a.project}/${a.name}\` on \`${t.name}\``);
  };
  const until = (l: Lease) => (l.expires_at ? `until ${l.expires_at}` : "until revoked");

  // ----- login sessions (docs/api.md §16.4) -----
  const logins = new Map<string, LoginSession & { name?: string }>();
  const LOGIN_URL: Record<LoginKind, string> = {
    claude: "https://claude.ai/oauth/authorize?code=true&client_id=mock&response_type=code&state=8f2c",
    codex: "https://auth.openai.com/codex/device",
    copilot: "https://github.com/login/device",
    pi: "https://auth.openai.com/oauth/authorize?client_id=mock-pi&state=77aa",
  };
  const DEVICE_CODE: Partial<Record<LoginKind, string>> = { codex: "QX7M-4KRP9", copilot: "8F3D-21AC" };
  const LOGIN_CMD: Record<LoginKind, string> = { claude: "claude auth login", codex: "codex login --device-auth", copilot: "copilot login --device-code", pi: "pi" };
  const INSTRUCTIONS = (kind: LoginKind, provider?: string) =>
    kind === "claude"
      ? "Open the URL, sign in, and paste the code that the page shows."
      : kind === "pi"
        ? `${PI_PLAN_REFUSAL} (${provider ?? "no provider"})`
        : "Open the URL, sign in, and enter the device code.";
  const DEFAULTS: Record<LoginKind, (p?: string) => Pick<Subscription, "provider" | "plan" | "harnesses" | "probe"> & { name: string }> = {
    claude: () => ({ name: "claude-pro", provider: "anthropic", plan: "Pro", harnesses: ["claude"], probe: "anthropic-oauth-usage" }),
    codex: () => ({ name: "chatgpt-plus", provider: "openai", plan: "ChatGPT Plus", harnesses: ["codex"], probe: "openai-codex-usage" }),
    copilot: () => ({ name: "copilot", provider: "github", plan: "Copilot Pro", harnesses: ["copilot"], probe: "github-copilot-quota" }),
    pi: (p) => ({ name: `pi-${p ?? "anthropic"}`, provider: p ?? "anthropic", harnesses: ["pi"], probe: undefined }),
  };
  const live = (id: string) => {
    const s = logins.get(id);
    if (!s || s.done) fail(409, "not_running", `login session \`${id}\` is not live`);
    return s!;
  };
  const endLogin = (s: LoginSession, state: "failed" | "cancelled" | "expired" | "done", error?: string) => {
    if (s.done) return;
    s.state = state;
    s.done = true;
    s.finished_at = iso();
    if (error) s.error = error;
    audit("login.end", s.id, undefined, { kind: s.kind, state });
  };
  const toWaiting = (s: LoginSession) => {
    if (s.state !== "starting") return;
    s.state = "waiting";
    s.url = LOGIN_URL[s.kind];
    if (DEVICE_CODE[s.kind]) {
      s.device_code = DEVICE_CODE[s.kind];
      const auto = opts.autoLoginMs ?? 8000;
      if (auto > 0) setTimeout(() => finishLogin(s), auto);
    }
  };
  /** The flow exited 0: verify, make or update the subscription, probe once. */
  const finishLogin = (s: LoginSession & { name?: string }) => {
    if (s.done || s.state === "verifying") return;
    s.state = "verifying";
    later(() => {
      if (s.done) return;
      const d = DEFAULTS[s.kind](s.provider);
      // PLAN §16.3: with no name, a default that has an account already gets a second account, `<default>-2`, then -3.
      let name = s.name ?? d.name;
      for (let n = 2; !s.name && w.subscriptions.find((x) => x.name === name)?.account; n++) name = `${d.name}-${n}`;
      let row = w.subscriptions.find((x) => x.name === name);
      if (!row) {
        row = { ...d, name, alert_at: 80, source: "api", login_status: "logged_in", probe_status: "none", created_at: iso(), updated_at: iso(), windows: [], local_estimate_only: !d.probe, has_api_key: false, held_agents: 0, agents: [] };
        w.subscriptions.push(row);
        audit("subscription.register", name, undefined, { kind: s.kind });
      }
      // A re-login replaces the login of this account only.
      row.account = { harness: s.kind, has_login: true, source: row.account?.source ?? "login", created_at: row.account?.created_at ?? iso(), updated_at: iso() };
      // PLAN §16.3: the first account of a harness becomes its default; a second keeps it.
      const kin = w.subscriptions.filter((x) => x.account?.harness === s.kind);
      if (DEFAULT_HARNESSES.includes(s.kind) && !kin.some((x) => x.default) && kin.every((x) => x === row)) setDefault(row, s.kind, "first_account", "system");
      row.login_status = "logged_in";
      row.verified_at = iso();
      row.updated_at = iso();
      if (row.probe) {
        if (!row.windows.length) row.windows = [{ window: s.kind === "copilot" ? "monthly" : "5h", used_pct: 3, resets_at: new Date(now() + 3 * 3600_000).toISOString(), at: iso() }];
        reading(row);
      }
      s.subscription = name;
      endLogin(s, "done");
    }, latency > 0 ? 1200 : 0);
  };
  const fakeLoginPty = (s: LoginSession | undefined, protocol: string | null) => {
    if (!s || s.done) return new FakePty({ refuse: true, protocol });
    const k = s.kind;
    const banner = [`\x1b[2m$ ${LOGIN_CMD[k]}\x1b[0m  \x1b[2m(mock login terminal)\x1b[0m`];
    if (k === "codex") banner.push("", "Welcome to Codex [v0.141.0]", "", "Follow these steps to sign in with ChatGPT using device code authorization:", "", `1. Open this link in your browser and sign in to your account`, `   \x1b[94m${LOGIN_URL.codex}\x1b[0m`, "", `2. Enter this one-time code \x1b[90m(expires in 15 minutes)\x1b[0m`, `   \x1b[94m${DEVICE_CODE.codex}\x1b[0m`, "");
    if (k === "copilot") banner.push("", `! First copy your one-time code: \x1b[1m${DEVICE_CODE.copilot}\x1b[0m`, `Then open: ${LOGIN_URL.copilot}`, "Waiting for authorization...");
    if (k === "claude") banner.push("", "Browser didn't open? Use the url below to sign in:", "", LOGIN_URL.claude, "", "Paste code here if prompted >");
    return new FakePty({
      protocol,
      prompt: "",
      banner,
      onLine: (line) => {
        if (s.done || !line.trim()) return [];
        finishLogin(s);
        return ["\x1b[32m✓ Login successful.\x1b[0m"];
      },
    });
  };

  // ----- memory (docs/api.md §10) -----
  const memNo = (id: string) => Number(id.split("-")[1]) || 0;
  const memProject = (id: string) => w.memories.find((m) => m.id === id)?.project;
  const memKey = (proj: string, id: string) => `${proj}/${id}`;
  /** `MEM-n` (unique across projects for the operator) or `project/MEM-n`. */
  const memory = (key: string, inProject?: string): Memory => {
    if (key.includes("/")) return find(w.memories, (m) => m.key === key, `memory \`${key}\``);
    const hits = w.memories.filter((m) => m.id === key && (!inProject || m.project === inProject));
    if (hits.length > 1) fail(400, "bad_request", `memory \`${key}\` is in more than one project; use project/${key}`);
    return hits[0] ?? notFound(`memory \`${key}\``);
  };
  const FIRST_REL = ["contradicts", "supersedes"];
  /** A memory with its `links` in both directions, contradicts first, then supersedes. */
  const memView = (m: Memory): Memory => {
    const title = (id: string) => w.memories.find((x) => x.project === m.project && x.id === id);
    const edges = w.links
      .filter((l) => memProject(l.from) === m.project && (l.from === m.id || l.to === m.id))
      .map((l) => {
        const other = l.from === m.id ? l.to : l.from;
        return { ...l, direction: l.from === m.id ? ("out" as const) : ("in" as const), other, other_title: title(other)?.title ?? other, other_superseded: !!title(other)?.superseded_by };
      })
      .sort((a, b) => Number(FIRST_REL.includes(b.relation)) - Number(FIRST_REL.includes(a.relation)) || (FIRST_REL.indexOf(a.relation) - FIRST_REL.indexOf(b.relation)) || a.created_at.localeCompare(b.created_at));
    return { ...m, links: edges };
  };
  const supersede = (old: Memory, next: Memory) => {
    if (old.superseded_by) fail(409, "conflict", `${old.id} is superseded by ${old.superseded_by} already`);
    if (next.superseded_by === old.id) fail(422, "invalid", `${next.id} is superseded by ${old.id}; it cannot supersede it back`);
    old.superseded_by = next.id;
    next.supersedes = [...new Set([...next.supersedes, old.id])];
    w.links.push({ from: next.id, to: old.id, relation: "supersedes", author: "operator", author_name: "operator", created_at: iso() });
    emit({ kind: "memory.superseded", actor: "operator", project: old.project, summary: `${next.id} supersedes ${old.id}`, data: { memory_id: old.id, key: old.key, title: old.title, by: next.id } });
  };

  // ----- wiki (docs/api.md §11) -----
  const page = (key: string): WikiPage => {
    const byId = w.pages.find((p) => p.id === key);
    if (byId) return byId;
    const [proj, ...rest] = key.split("/");
    if (proj && rest.length && w.projects.some((p) => p.name === proj)) {
      if (rest.join("/") === "index") ensureIndex(proj);
      return find(w.pages, (p) => p.project === proj && p.path === rest.join("/") && !p.deleted_at, `page \`${key}\``);
    }
    return find(w.pages, (p) => p.path === key && !p.deleted_at, `page \`${key}\``);
  };
  const ensureIndex = (proj: string) => {
    if (w.pages.some((p) => p.project === proj && p.path === "index" && !p.deleted_at)) return;
    const body = `# ${proj}\n\nThe wiki of project \`${proj}\`. Keep this page as the table of contents.\n`;
    const pg: WikiPage = { id: ulid("pg"), project: proj, path: "index", title: proj, rev: 1, body, author: "system", author_name: "system", summary: "Create the index page", created_by: "system", created_at: iso(), updated_at: iso(), links: [] };
    w.pages.push(pg);
    w.revisions[pg.id] = [{ page: pg.id, rev: 1, author: "system", author_name: "system", summary: pg.summary, created_at: iso(), body }];
  };
  /** Title, status, and reviewer come from the body (docs/api.md §11.1). */
  const applyBody = (pg: WikiPage, body: string, title?: string) => {
    const { meta, body: rest } = splitFrontMatter(body);
    pg.body = body;
    pg.title = title || meta.title || /^#\s+(.+)$/m.exec(rest)?.[1]?.trim() || pg.path.split("/").pop()!;
    const st = meta.status;
    if (st && st !== "draft" && st !== "ready_for_review" && st !== "reviewed") fail(422, "invalid", `\`${st}\` is not a page status`);
    if (st) pg.status = st as WikiPage["status"];
    else delete pg.status;
    if (meta.reviewer) pg.reviewer = meta.reviewer;
    else delete pg.reviewer;
    pg.links = [...body.matchAll(/\[\[([^\]|]+)(?:\|[^\]]*)?\]\]/g)].map((m) => {
      const t = m[1]!;
      if (/^[A-Z][A-Z0-9]*-\d+$/.test(t)) return t.startsWith("MEM-") ? { kind: "memory", target: `${pg.project}/${t}` } : { kind: "card", target: t };
      if (t.startsWith("project:")) return { kind: "page", target: t.slice(8) };
      return { kind: "page", target: `${pg.project}/${t}` };
    });
    for (const m of body.matchAll(/artifact[:\s]+(ART-\d+)/g)) pg.links.push({ kind: "artifact", target: `${pg.project}/${m[1]}` });
  };
  const newRevision = (pg: WikiPage, body: string, summary: string, title?: string) => {
    applyBody(pg, body, title);
    pg.rev++;
    pg.summary = summary;
    pg.author = "operator";
    pg.author_name = "operator";
    pg.updated_at = iso();
    (w.revisions[pg.id] ??= []).push({ page: pg.id, rev: pg.rev, author: "operator", author_name: "operator", summary, created_at: iso(), body });
  };
  const summaryOf = (p: WikiPage, needle?: string): WikiPageSummary => {
    const { body: _b, links: _l, summary: _s, created_by: _c, created_at: _ca, ...rest } = p;
    const i = needle ? p.body.toLowerCase().indexOf(needle) : -1;
    return i >= 0 ? { ...rest, snippet: p.body.slice(Math.max(0, i - 30), i + 60) } : rest;
  };
  const wikiEvent = (kind: AgentEvent["kind"], pg: WikiPage, summary: string, extra: Record<string, unknown> = {}) =>
    emit({ kind, actor: "operator", project: pg.project, summary, data: { page_id: pg.id, path: pg.path, title: pg.title, rev: pg.rev, ...extra } });

  // ----- artifacts (docs/api.md §12) -----
  const artNo = (id: string) => Number(id.split("-")[1]) || 0;
  const artifact = (key: string): Artifact => {
    const hit = w.artifacts.find((a) => a.uid === key || a.key === key);
    if (hit) return hit;
    const ids = w.artifacts.filter((a) => a.id === key);
    if (ids.length > 1) fail(400, "bad_request", `artifact \`${key}\` is in more than one project; use project/${key}`);
    return ids[0] ?? notFound(`artifact \`${key}\``);
  };
  const artView = (a: Artifact): Artifact => ({
    ...a,
    embedded_in: w.pages.filter((p) => p.project === a.project && !p.deleted_at && new RegExp(`artifact[:\\s]+${a.id}\\b`).test(p.body)).map((p) => `${p.project}/${p.path}`),
  });
  /** The link of an artifact (store/artifacts.rs link_row): a page, a card, or a label. A version is `422` since TER-58. */
  const linkOf = (kind: string, ref: string): { kind: ArtifactLinkKind; ref: string } => {
    if (kind === "version") fail(422, "invalid", "version objects are removed (TER-58): link a version label, kind `label` with ref `version:<X.Y.Z>`");
    if (kind !== "page" && kind !== "card" && kind !== "label") fail(422, "invalid", `bad link kind \`${kind}\`: use page, card, or label`);
    const r = kind === "label" ? ref.trim() : ref;
    if (kind === "label" && (!r || [...r].length > 200)) fail(422, "invalid", `bad label \`${r}\`: use 1-200 characters, no control characters`);
    return { kind, ref: r };
  };

  const derivePrefix = (name: string) => {
    let letters = name.replace(/[^a-z0-9]/gi, "").toUpperCase();
    if (!/^[A-Z]/.test(letters)) letters = `P${letters}`;
    letters = letters.slice(0, 10);
    const taken = (p: string) => w.projects.some((x) => x.card_prefix === p);
    for (let len = 3; len <= Math.max(3, letters.length); len++) {
      const c = letters.slice(0, len);
      if (!taken(c)) return c;
    }
    for (let n = 2; ; n++) if (!taken(`${letters.slice(0, 3)}${n}`)) return `${letters.slice(0, 3)}${n}`;
  };

  // ----- sessions (docs/api.md §1.1) -----
  type MockCookie = { csrf: string; exp: number };
  const jar = opts.cookieJar ?? null;
  let cookie: MockCookie | null = (() => {
    try {
      const raw = jar?.getItem(MOCK_COOKIE_KEY);
      return raw ? (JSON.parse(raw) as MockCookie) : null;
    } catch {
      return null;
    }
  })();
  const saveCookie = () => {
    try {
      if (cookie) jar?.setItem(MOCK_COOKIE_KEY, JSON.stringify(cookie));
      else jar?.removeItem(MOCK_COOKIE_KEY);
    } catch {
      // ignore
    }
  };
  /** The cookie the browser still sends: it drops the cookie at its end (`Max-Age`). */
  const liveCookie = (): MockCookie | null => {
    if (cookie && cookie.exp <= now()) {
      cookie = null;
      saveCookie();
    }
    return cookie;
  };
  const unauthorized = (message: string): never => {
    opts.client?.onUnauthorized?.();
    throw new AuthError({ error: { code: "unauthorized", message } });
  };
  const CSRF_REFUSED = "a request with the session cookie must send X-CSRF-Token with the csrf value of the session (POST or GET /api/session)";
  let password: string | null = opts.password === undefined ? MOCK_PASSWORD : opts.password;
  /** The failed password logins, as the daemon's limiter counts them (5 free, then 30 s and more). */
  let failures = 0;
  let lockedUntil = 0;
  const newCookie = (): SessionGrant => {
    const exp = Math.floor((now() + (opts.sessionTtlMs ?? 30 * 86_400_000)) / 1000);
    cookie = { csrf: `csrf_${ulid("csrf").toLowerCase()}`, exp: exp * 1000 };
    saveCookie();
    return { csrf: cookie.csrf, exp, expires_at: new Date(exp * 1000).toISOString().replace(/\.\d{3}Z$/, "Z"), kind: "browser" };
  };
  /** The TCP listener's check: the bearer token first, then the cookie (with CSRF on a mutating call). */
  const authorize = (verb: string) => {
    const c = opts.client;
    if (!c) return;
    const bearer = c.token();
    if (bearer !== null) {
      if (bearer !== MOCK_TOKEN) unauthorized("missing or wrong bearer token");
      return;
    }
    const ck = liveCookie();
    if (!ck) unauthorized("missing or wrong bearer token");
    if (isMutating(verb) && c.csrf() !== ck!.csrf) fail(403, "forbidden", CSRF_REFUSED);
  };
  /** What the mock "browser" offers on an upgrade. */
  const wsAuth = (): MockSocketAuth | undefined =>
    opts.client ? { token: opts.client.token(), cookie: liveCookie() !== null } : undefined;
  const sock: typeof mockSocket = (path, make) => mockSocket(path, make, wsAuth());

  const api: Api & { world: MockWorld } = {
    world: w,
    session: {
      create: (token) =>
        reply(() => {
          // The daemon answers 404; the HTTP client turns that into null.
          if (opts.sessions === false) return null;
          if (token !== MOCK_TOKEN) throw new AuthError({ error: { code: "unauthorized", message: "missing or wrong bearer token" } });
          const g = newCookie();
          return { csrf: g.csrf, exp: g.exp, expires_at: g.expires_at };
        }),
      login: (pw) =>
        reply(() => {
          if (opts.sessions === false) fail(404, "not_found", "no such API route");
          if (lockedUntil > now()) {
            const wait = Math.ceil((lockedUntil - now()) / 1000);
            fail(429, "rate_limited", `too many failed logins from this address; try again in ${wait} s`, { retry_after: wait });
          }
          if (password === null) fail(409, "conflict", "no password set; run terrariumd password set");
          if (pw !== password) {
            failures += 1;
            if (failures >= 5) lockedUntil = now() + Math.min(30_000 * 2 ** (failures - 5), 900_000);
            throw new AuthError({ error: { code: "unauthorized", message: "login failed" } });
          }
          failures = 0;
          lockedUntil = 0;
          return newCookie();
        }),
      current: () =>
        reply((): SessionInfo | null => {
          const c = opts.client;
          if (!c) return { kind: "local", password_set: password !== null };
          const bearer = c.token();
          if (bearer !== null) return bearer === MOCK_TOKEN ? { kind: "token", password_set: password !== null } : null;
          const ck = liveCookie();
          if (!ck) return null;
          return { kind: "browser", csrf: ck.csrf, exp: ck.exp / 1000, expires_at: new Date(ck.exp).toISOString().replace(/\.\d{3}Z$/, "Z"), password_set: password !== null };
        }),
      remove: () =>
        reply(() => {
          if (opts.sessions === false) return;
          const ck = liveCookie();
          if (ck && opts.client && opts.client.csrf() !== ck.csrf) fail(403, "forbidden", CSRF_REFUSED);
          cookie = null;
          saveCookie();
        }),
    },
    account: {
      setPassword: (change) =>
        reply(() => {
          authorize("PUT");
          const viaCookie = opts.client !== undefined && opts.client.token() === null;
          if (password !== null && (change.current !== undefined || viaCookie) && change.current !== password) {
            fail(403, "forbidden", change.current === undefined ? "a session must send the current password as `current`" : "the current password is wrong");
          }
          if ([...(change.new ?? "")].length < 12) fail(422, "invalid", "the password must have at least 12 characters");
          password = change.new;
          // A new password version: every session ends.
          cookie = null;
          saveCookie();
        }),
      logoutEverywhere: () =>
        reply(() => {
          authorize("DELETE");
          cookie = null;
          saveCookie();
        }),
    },
    selfUpdate: {
      status: () => reply(() => selfUpdateReport()),
      check: () =>
        reply(() => {
          updater.checkedAt = iso();
          return selfUpdateReport();
        }),
      apply: (version) =>
        reply(() => {
          const target = version ?? selfUpdateLatest()?.version ?? "latest";
          if (!/^v?[0-9A-Za-z][0-9A-Za-z._-]*$/.test(target)) fail(422, "invalid", `\`${target}\` is not a version, \`latest\`, or \`pre\``);
          const v = target === "latest" || target === "pre" ? (selfUpdateLatest()?.version ?? updater.running) : target.replace(/^v/, "");
          const to: UpdaterImage = { reference: `ghcr.io/wiltaylor/terrarium@sha256:${v.replace(/\W/g, "")}`, id: `sha256:${v}`, digest: `sha256:${v}`, version: v };
          const run = mockSwitch("update", target, to);
          audit("system.update", target, undefined, { target, run: run.id, ok: true });
          return { run: run.id, target };
        }),
      rollback: () =>
        reply(() => {
          if (!updater.previous) fail(409, "conflict", "no switch to roll back: the updater has no previous image");
          const run = mockSwitch("rollback", "previous", updater.previous!);
          audit("system.rollback", "previous", undefined, { run: run.id, ok: true });
          return { run: run.id };
        }),
    },
    systemVersion: () =>
      reply(() => {
        const [cargo_version = "", git_ref = "unknown"] = MOCK_DAEMON_VERSION.split("+");
        return { version: MOCK_DAEMON_VERSION, cargo_version, git_ref, dirty: false, build_time: new Date(startedAt - 86_400_000).toISOString().replace(/\.\d+Z$/, "Z") };
      }),
    health: () =>
      reply(() => ({
        status: "ok",
        version: MOCK_DAEMON_VERSION,
        uptime: Math.round((now() - startedAt) / 1000) + 3600,
        started_at: new Date(startedAt - 3600_000).toISOString(),
        definitions: { projects: 1, agents: 1, targets: 0, subscriptions: 0 },
        idle_stop: MOCK_IDLE_STOP,
        compact_on_idle: false,
        compact_max_age: "off",
        compact_harnesses: [...MOCK_COMPACT_HARNESSES],
      })),
    harnesses: (_refresh, latest) => reply(() => harnessReport(latest === true)),
    harnessUpdate: (name, o = {}) =>
      reply(() => {
        const spec = MOCK_HARNESSES.find((h) => h.name === name);
        if (!spec) fail(422, "invalid", `unknown harness \`${name}\`: use one of ${MOCK_HARNESSES.map((h) => h.name).join(", ")}`);
        const version = checkHarnessVersion(o.version);
        return updateHarness(spec, version, o.force === true);
      }),
    harnessPin: (name, version) =>
      reply((): HarnessPinResult => {
        const spec = MOCK_HARNESSES.find((h) => h.name === name && h.harness !== null);
        if (!spec) fail(422, "invalid", `unknown harness \`${name}\`: use one of claude, codex, copilot, pi`);
        const active = harnessInstalled[name] ?? null;
        const v = version ?? harnessPrevious(name, active);
        if (!v) fail(422, "invalid", `${name} has no previous version on disk: give a version (\`terrarium admin harness pin ${name} <version>\`)`);
        if (!VERSION_RE_STRICT.test(v)) fail(422, "invalid", `\`${v}\` is not a version like 1.2.3`);
        const r = updateHarness(spec, v, false, "pin");
        const p = harnessPolicy[name]!;
        p.policy = "pinned";
        p.pinned_version = r.to;
        const names = r.agents_on_old_version.map((a) => a.agent);
        audit("harness.pin", name, undefined, { version: r.to, from: r.from, ok: true, agents_on_old_version: names });
        emit({ kind: "harness", actor: "operator", summary: `${name} pinned to ${r.to}${r.from !== r.to ? " (rollback)" : ""}`, data: { change: "pinned", harness: name, package: spec.package, from: r.from, to: r.to, agents_on_old_version: names } });
        return { harness: name, policy: "pinned", pinned_version: r.to, from: r.from, to: r.to, ...(r.reused ? {} : { output: r.output }), agents_on_old_version: r.agents_on_old_version, ...(r.note ? { note: r.note } : {}), ...(r.removed ? { removed: r.removed } : {}) };
      }),
    harnessUnpin: (name) =>
      reply((): HarnessPinResult => {
        const p = harnessPolicy[name];
        if (!p) fail(422, "invalid", `unknown harness \`${name}\`: use one of claude, codex, copilot, pi`);
        const was = p.pinned_version;
        p.policy = "auto";
        p.pinned_version = null;
        audit("harness.unpin", name, undefined, { was });
        emit({ kind: "harness", actor: "operator", summary: `${name} unpinned: auto-update`, data: { change: "unpinned", harness: name, was } });
        const active = harnessInstalled[name] ?? null;
        return { harness: name, policy: "auto", pinned_version: null, from: active, to: active, agents_on_old_version: [] };
      }),
    harnessCheck: () =>
      reply(() => {
        const at = iso();
        const results = MOCK_HARNESSES.filter((h) => h.harness !== null).map((h): HarnessCheckItem => {
          const p = harnessPolicy[h.name]!;
          const from = harnessInstalled[h.name] ?? null;
          const latest = "error" in h.npm ? null : h.npm.version;
          let item: HarnessCheckItem;
          if (p.policy === "pinned") item = { harness: h.name, result: "pinned", detail: `pinned to ${p.pinned_version}; no auto-update`, latest, mailed: false };
          else if (latest === null) item = { harness: h.name, result: "skipped", detail: "error" in h.npm ? h.npm.error : "", latest, mailed: false };
          else if (!from || harnessUpdateKind(from, latest) === "none") item = { harness: h.name, result: "current", detail: `${from} is the latest release`, latest, mailed: false };
          else {
            updateHarness(h, latest, false, "auto");
            // TERRARIUM_HARNESS_UPDATE_MAIL is `off` (the default): an installed update sends no mail.
            item = { harness: h.name, result: "updated", detail: `${from} -> ${latest}`, from, to: latest, latest, mailed: false };
            audit("harness.auto_update", h.name, undefined, { package: h.package, from, to: latest, latest, ok: true, detail: item.detail }, "system");
            emit({ kind: "harness", actor: "system", summary: `${h.name} ${from} -> ${latest} (auto-update)`, data: { change: "auto_updated", harness: h.name, package: h.package, from, to: latest } });
          }
          Object.assign(p, { checked_at: at, result: item.result, detail: item.detail, latest: latest ?? p.latest });
          return item;
        });
        return { checked_at: at, results };
      }),
    harnessUpdateAll: (o = {}) =>
      reply(() => {
        // One after another; a failed item gets `error` and its npm output, and the rest go on.
        const results = MOCK_HARNESSES.map((h): HarnessUpdateResult => {
          try {
            return updateHarness(h, "latest", o.force === true);
          } catch (e) {
            const d = (e as ApiError).details as { output?: string } | undefined;
            return { harness: h.name, package: h.package, from: harnessInstalled[h.name] ?? null, to: null, output: d?.output ?? "", reused: false, agents_on_old_version: [], error: (e as Error).message };
          }
        });
        const failed = results.filter((r) => r.error).length;
        if (failed) fail(422, "invalid", `${failed} of ${results.length} updates failed`, { results });
        return { results };
      }),

    projects: {
      list: () => reply(() => w.projects.slice().sort((a, b) => a.name.localeCompare(b.name))),
      get: (name) => reply(() => project(name)),
      create: (p) =>
        reply(() => {
          if (!NAME_RE.test(p.name)) fail(422, "invalid", `\`${p.name}\` is not a valid name: 1 to 63 of a-z, 0-9, - and _, starting with a letter or a digit`);
          if (w.projects.some((x) => x.name === p.name)) fail(409, "conflict", `project \`${p.name}\` exists`);
          if (p.card_prefix !== undefined && !PREFIX_RE.test(p.card_prefix)) fail(422, "invalid", `\`${p.card_prefix}\` is not a valid card prefix`);
          if (p.card_prefix && w.projects.some((x) => x.card_prefix === p.card_prefix)) fail(409, "conflict", `card prefix \`${p.card_prefix}\` is taken`);
          const proj: Project = { name: p.name, description: p.description ?? "", card_prefix: p.card_prefix || derivePrefix(p.name), source: "api", created_at: iso(), updated_at: iso() };
          w.projects.push(proj);
          w.columns[p.name] = [
            { name: "backlog", position: 0 },
            { name: "ready", position: 1 },
            { name: "in_progress", position: 2, role: "working" },
            { name: "review", position: 3 },
            { name: "blocked", position: 4, role: "blocked" },
            { name: "done", position: 5 },
          ];
          audit("project.create", proj.name, proj.name, { card_prefix: proj.card_prefix, source: "api" });
          return proj;
        }),
      update: (name, patch) =>
        reply(() => {
          const p = project(name);
          if (p.source === "file") fail(409, "file_managed", `project \`${name}\` is declared in a definition file; edit the file, then POST /api/reload`);
          if (patch.card_prefix !== undefined && patch.card_prefix !== p.card_prefix) {
            if (!PREFIX_RE.test(patch.card_prefix)) fail(422, "invalid", `\`${patch.card_prefix}\` is not a valid card prefix`);
            if (w.cards.some((c) => c.project === name)) fail(409, "conflict", "the card prefix cannot change after the first card");
            if (w.projects.some((x) => x.card_prefix === patch.card_prefix)) fail(409, "conflict", `card prefix \`${patch.card_prefix}\` is taken`);
            p.card_prefix = patch.card_prefix;
          }
          if (patch.description !== undefined) p.description = patch.description;
          p.updated_at = iso();
          audit("project.update", name, name, { ...patch });
          return p;
        }),
      remove: (name) =>
        reply(() => {
          const p = project(name);
          if (p.source === "file") fail(409, "file_managed", `project \`${name}\` is declared in a definition file`);
          if (w.agents.some((a) => a.project === name)) fail(409, "conflict", `project \`${name}\` has agents; delete them first`);
          w.projects = w.projects.filter((x) => x.name !== name);
          w.repos = w.repos.filter((r) => r.project !== name);
          folderMock.onProjectRemoved(name);
          w.cards = w.cards.filter((c) => c.project !== name);
          delete w.columns[name];
          audit("project.delete", name, name, {});
        }),
      agents: (name) => reply(() => (project(name), w.agents.filter((a) => a.project === name).sort((a, b) => a.id.localeCompare(b.id)))),
      targets: (name) => reply(() => (project(name), w.targets.filter((t) => !t.project || t.project === name).map(targetView))),
      // The seed grants the global `pi-runner` to `terrarium` for its project-owned monitors.
      targetGrants: (name) =>
        reply(() => (project(name), name === "terrarium" ? [{ target: "pi-runner", project: name, for: "monitors" as const, granted_by: "operator", granted_at: new Date(now() - 86_400_000).toISOString() }] : [])),
      repos: (name) => reply(() => (project(name), w.repos.filter((r) => r.project === name).sort((a, b) => a.name.localeCompare(b.name)))),
      attachRepo: (name, r) =>
        reply(() => {
          project(name);
          if (!r.url?.trim()) fail(422, "invalid", "a repo needs a url");
          const repoName = r.name || r.url.replace(/\/+$/, "").split(/[/:]/).pop()!.replace(/\.git$/, "");
          if (w.repos.some((x) => x.project === name && x.name === repoName)) fail(409, "conflict", `repo \`${repoName}\` exists in \`${name}\``);
          const repo: Repo = { id: ulid("repo"), project: name, name: repoName, url: r.url, ...(r.default_branch ? { default_branch: r.default_branch } : {}), ...(r.credential ? { credential: r.credential } : {}), credential_admin: true, path: r.path || repoName, source: "api", created_at: iso() };
          w.repos.push(repo);
          audit("repo.attach", repo.id, name, { name: repoName, url: r.url });
          return repo;
        }),
    },

    // Checkout: the mock has no workspaces, so the same 404 as an unknown route.
    // The browser (docs/api.md §5.3) reads a fixture (./repoTrees).
    repos: {
      checkout: () => reply(noRoute),
      trees: (id) =>
        reply(() => {
          const r = find(w.repos, (x) => x.id === id || `${x.project}/${x.name}` === id, `repo \`${id}\``);
          return { repo: r.name, repo_id: r.id, project: r.project, default_branch: r.default_branch ?? "main", trees: mockTrees(r.name) };
        }),
      files: (id, tree, path) => reply(() => mockList(repoName(id), tree, path)),
      file: async (id, tree, path) => {
        const f = await reply(() => mockFile(repoName(id), tree, path));
        const bytes = f.bytes.slice(0, 1 << 20);
        return { blob: new Blob([bytes], { type: f.text ? "text/plain; charset=utf-8" : "application/octet-stream" }), text: f.text, size: f.bytes.length, truncated: bytes.length < f.bytes.length };
      },
      diff: (id, tree) => reply(() => mockChanges(repoName(id), tree)),
      commits: (id, tree, cursor) => reply(() => mockCommits(repoName(id), tree, cursor)),
      commit: (id, tree, sha) => reply(() => mockCommit(repoName(id), tree, sha)),
    },

    agents: {
      // The ephemeral agents only with `?include=ephemeral` (docs/api.md §6.10), the card agents among them with `?include=card` (TER-75).
      list: (proj, o = {}) => reply(() => w.agents.filter((a) => (!proj || a.project === proj) && (o.ephemeral || !a.ephemeral || (o.card && !!a.current_card))).sort((a, b) => a.id.localeCompare(b.id)).map(withSkills)),
      get: (id) =>
        reply(() => {
          const a = withSkills(agent(id));
          const s = a.subscription ? w.subscriptions.find((x) => x.name === a.subscription) : undefined;
          const open = waitMock.openWaits(a);
          const detail = { ...a, ...(open.length ? { open_waits: open } : {}) };
          return s ? { ...detail, subscription_detail: subscriptionDetail(s, a.harness) } : detail;
        }),
      create: (body) =>
        reply(() => {
          project(body.project);
          // docs/api.md §21: the template's fields, and the body's on top. The checks below see the result.
          const tpl = "template" in body && body.template ? resolveTemplate(w.templates, body.template, body.project, fail) : undefined;
          const def = tpl ? applyTemplate(tpl.spec, body) : (body as AgentDef);
          if (!def.harness) fail(422, "invalid", "missing field `harness`");
          const sp = systemPromptError(def.system_prompt);
          if (sp) fail(422, "invalid", sp, { field: "system_prompt" });
          const pe = personaError(def.persona);
          if (pe) fail(422, "invalid", pe, { field: "persona" });
          const ie = idleStopError(def.idle_stop);
          if (ie) fail(422, "invalid", ie, { field: "idle_stop" });
          const ce = compactOnIdleError(def.compact_on_idle) ?? compactMaxAgeError(def.compact_max_age);
          if (ce) fail(422, "invalid", ce);
          const sk = skillsError(def.skills, skillMock.names());
          if (sk) fail(422, "invalid", sk, { field: "skills" });
          if (!NAME_RE.test(def.name)) fail(422, "invalid", `\`${def.name}\` is not a valid name`);
          if (w.agents.some((a) => a.project === def.project && a.name === def.name)) fail(409, "conflict", `agent \`${def.project}/${def.name}\` exists`);
          if (def.policy?.mail_to.some((m) => !m.trim())) fail(422, "invalid", "policy.mail_to has an empty entry");
          for (const x of def.policy?.cross_project ?? []) if (!/^[^/]+\/[^/]+$/.test(x)) fail(422, "invalid", `policy.cross_project entry \`${x}\` is not project/agent`);
          const pool = checkPool(def.harness, def.subscription, def.subscription_pool ?? []);
          const dis = disabledRefusal(def.subscription);
          if (dis) fail(422, "invalid", dis);
          const id = ulid("agt");
          const a: Agent = {
            id,
            name: def.name,
            project: def.project,
            harness: def.harness,
            ...(def.subscription ? { subscription: def.subscription } : {}),
            ...(pool.length ? { subscription_pool: pool } : {}),
            ...(def.model ? { model: def.model } : {}),
            ...(def.prompt ? { prompt: def.prompt } : {}),
            ...(def.system_prompt ? { system_prompt: def.system_prompt } : {}),
            ...(def.persona?.trim() ? { persona: def.persona.trim() } : {}),
            ...(def.skills?.length ? { skills: [...def.skills] } : {}),
            ...(def.skills_off?.length ? { skills_off: [...def.skills_off] } : {}),
            ...(def.default_skills === false ? { default_skills: false } : {}),
            workspace: `/state/agents/${id}/workspace`,
            roles: (def.roles ?? []).filter((r): r is Role => r !== "member"),
            // Like the daemon: `cross_project` is left out when empty.
            policy: { targets: def.policy?.targets ?? [], mail_to: def.policy?.mail_to ?? ["*"], ...(def.policy?.cross_project?.length ? { cross_project: def.policy.cross_project } : {}) },
            restart: def.restart ?? "on-failure",
            ...(def.idle_stop?.trim() ? { idle_stop: def.idle_stop.trim() } : {}),
            ...(typeof def.compact_on_idle === "boolean" ? { compact_on_idle: def.compact_on_idle } : {}),
            ...(def.compact_max_age?.trim() ? { compact_max_age: def.compact_max_age.trim() } : {}),
            compact_supported: MOCK_COMPACT_HARNESSES.includes(def.harness),
            paused: false,
            status: "stopped",
            source: "api",
            ...(tpl ? { template: templateRef(tpl) } : {}),
            created_at: iso(),
            updated_at: iso(),
          };
          w.agents.push(a);
          audit("agent.create", a.id, a.project, { name: a.name, harness: a.harness, ...(tpl ? { template: templateRef(tpl) } : {}) });
          return withSkills(a);
        }),
      update: (id, patch) =>
        reply(() => {
          const a = agent(id);
          if (a.source === "file") fail(409, "file_managed", `agent \`${a.project}/${a.name}\` is declared in a definition file; edit the file, then POST /api/reload`);
          const sp = systemPromptError(patch.system_prompt);
          if (sp) fail(422, "invalid", sp, { field: "system_prompt" });
          const pe = personaError(patch.persona);
          if (pe) fail(422, "invalid", pe, { field: "persona" });
          const ie = idleStopError(patch.idle_stop);
          if (ie) fail(422, "invalid", ie, { field: "idle_stop" });
          const ce = compactOnIdleError(patch.compact_on_idle) ?? compactMaxAgeError(patch.compact_max_age);
          if (ce) fail(422, "invalid", ce);
          const sk = skillsError(patch.skills, skillMock.names());
          if (sk) fail(422, "invalid", sk, { field: "skills" });
          if (patch.subscription_pool !== undefined || patch.subscription !== undefined) {
            const current = patch.subscription !== undefined ? patch.subscription || undefined : a.subscription;
            const pool = checkPool(patch.harness ?? a.harness, current, patch.subscription_pool ?? a.subscription_pool ?? []);
            if (pool.length) a.subscription_pool = pool;
            else delete a.subscription_pool;
          }
          if (patch.harness) checkPiPlans(patch.harness, [patch.subscription !== undefined ? patch.subscription || undefined : a.subscription, ...(patch.subscription_pool ?? a.subscription_pool ?? [])]);
          if (patch.harness) a.harness = patch.harness;
          if (patch.subscription !== undefined) a.subscription = patch.subscription || undefined;
          if (patch.model !== undefined) a.model = patch.model || undefined;
          if (patch.prompt !== undefined) a.prompt = patch.prompt || undefined;
          // Applies at the next start, like the daemon.
          if (patch.system_prompt !== undefined) a.system_prompt = patch.system_prompt || undefined;
          // null or empty: by role.
          if (patch.persona !== undefined) a.persona = patch.persona?.trim() || undefined;
          // `[]` clears. A live agent keeps its skills until the next start.
          if (patch.skills !== undefined && JSON.stringify(patch.skills) !== JSON.stringify(a.skills ?? [])) {
            if (patch.skills.length) a.skills = [...patch.skills];
            else delete a.skills;
            if (LIVE.includes(a.status)) a.skills_restart = true;
          }
          if (patch.skills_off !== undefined && JSON.stringify(patch.skills_off) !== JSON.stringify(a.skills_off ?? [])) {
            if (patch.skills_off.includes("*")) fail(422, "invalid", "`*` is not valid in `skills_off`; to turn off every default skill, set `default_skills` to false");
            if (patch.skills_off.length) a.skills_off = [...patch.skills_off];
            else delete a.skills_off;
            if (LIVE.includes(a.status)) a.skills_restart = true;
          }
          if (patch.default_skills !== undefined && patch.default_skills !== (a.default_skills ?? true)) {
            if (patch.default_skills) delete a.default_skills;
            else a.default_skills = false;
            if (LIVE.includes(a.status)) a.skills_restart = true;
          }
          if (patch.policy) a.policy = { targets: patch.policy.targets, mail_to: patch.policy.mail_to, ...(patch.policy.cross_project?.length ? { cross_project: patch.policy.cross_project } : {}), ...(patch.policy.spawn ? { spawn: patch.policy.spawn } : {}) };
          if (patch.restart) a.restart = patch.restart;
          // null or empty: the global default.
          if (patch.idle_stop !== undefined) {
            if (patch.idle_stop?.trim()) a.idle_stop = patch.idle_stop.trim();
            else delete a.idle_stop;
          }
          if (patch.compact_on_idle !== undefined) {
            if (typeof patch.compact_on_idle === "boolean") a.compact_on_idle = patch.compact_on_idle;
            else delete a.compact_on_idle;
          }
          if (patch.compact_max_age !== undefined) {
            if (patch.compact_max_age?.trim()) a.compact_max_age = patch.compact_max_age.trim();
            else delete a.compact_max_age;
          }
          if (patch.harness) a.compact_supported = MOCK_COMPACT_HARNESSES.includes(patch.harness);
          a.updated_at = iso();
          audit("agent.update", a.id, a.project, { ...patch });
          return withSkills(a);
        }),
      // PLAN §16.3: move the agent to another account of its pool.
      swapSubscription: (id, to) =>
        reply(() => {
          const a = agent(id);
          const pool = a.subscription_pool ?? [];
          if (a.source === "file" && !pool.includes(to)) fail(409, "file_managed", `agent \`${a.project}/${a.name}\` is declared in a definition file; it may swap only among the accounts of its pool`);
          if (a.subscription === to) fail(409, "conflict", `agent \`${a.project}/${a.name}\` is on \`${to}\` already`);
          if (!pool.includes(to)) fail(422, "invalid", `\`${to}\` is not in the pool of agent \`${a.project}/${a.name}\``);
          const target = sub(to);
          const from = a.subscription ? w.subscriptions.find((x) => x.name === a.subscription) : undefined;
          if (from && from.provider !== target.provider) fail(422, "invalid", `\`${to}\` is ${target.provider}, the agent's account is ${from.provider}`);
          if (!target.account?.has_login || !target.harnesses.includes(a.harness)) fail(422, "invalid", `\`${to}\` has no ${a.harness} login`);
          const live = LIVE.includes(a.status);
          const applied: SwapResult["applied"] = !live ? "next_start" : a.harness === "claude" || a.harness === "pi" ? "live" : a.status === "working" ? "restart_scheduled" : "restarted";
          const result: SwapResult = { agent: a.id, ...(a.subscription ? { from: a.subscription } : {}), to, applied };
          a.subscription = to;
          a.updated_at = iso();
          audit("agent.swap", a.id, a.project, { from: result.from ?? null, to, applied });
          return result;
        }),
      remove: (id, confirm) =>
        reply(() => {
          const a = agent(id);
          // An ephemeral agent: the delete is a cancel.
          if (spawnMock.cancelOnRemove(a)) return;
          if (a.source === "file") fail(409, "file_managed", `agent \`${a.project}/${a.name}\` is declared in a definition file`);
          if (LIVE.includes(a.status) && !confirm) fail(409, "confirm_required", `agent \`${a.project}/${a.name}\` has a live session; send ?confirm=true`);
          removeAgentRow(a);
          // Its ephemeral agents go with it.
          spawnMock.ownerRemoved(a.id);
        }),
      // 202: the answer shows the agent as the daemon accepted the call. The
      // fake supervisor then steps through the statuses, as the daemon's does.
      start: (id) =>
        reply(() => {
          const a = agent(id);
          if (a.paused) fail(409, "conflict", `agent \`${a.project}/${a.name}\` is paused; unpause it first`);
          const dis = disabledRefusal(a.subscription);
          if (dis) fail(409, "conflict", dis);
          if (LIVE.includes(a.status)) fail(409, "already_running", `agent \`${a.project}/${a.name}\` is running already`);
          const snap = clone(a);
          audit("agent.start", a.id, a.project);
          later(() => {
            openSession(a);
            setStatus(a, "starting");
            later(() => {
              setStatus(a, waitMock.restStatus(a));
              flushQueue(a);
            });
          }, 0);
          return snap;
        }),
      stop: (id) =>
        reply(() => {
          const a = agent(id);
          if (!LIVE.includes(a.status)) fail(409, "not_running", `agent \`${a.project}/${a.name}\` is not running`);
          const snap = clone(a);
          audit("agent.stop", a.id, a.project);
          later(() => {
            endSession(a, "stopped");
            setStatus(a, "stopped", "stopped");
          }, 0);
          return snap;
        }),
      restart: (id) =>
        reply(() => {
          const a = agent(id);
          const dis = disabledRefusal(a.subscription);
          if (dis) fail(409, "conflict", dis);
          const snap = clone(a);
          audit("agent.restart", a.id, a.project);
          later(() => {
            const wasLive = LIVE.includes(a.status);
            if (wasLive) setStatus(a, "restarting");
            openSession(a);
            setStatus(a, "starting");
            later(() => {
              setStatus(a, waitMock.restStatus(a));
              flushQueue(a);
            });
          }, 0);
          return snap;
        }),
      // 202, idempotent. A live agent stops; nothing wakes it until the unpause.
      pause: (id) =>
        reply(() => {
          const a = agent(id);
          if (a.ephemeral) fail(409, "conflict", `\`${a.name}\` is an ephemeral agent: it is not paused. Cancel it with \`terrarium agent cancel ${a.name}\``);
          if (!a.paused) {
            a.paused = true;
            a.updated_at = iso();
            audit("agent.pause", a.id, a.project);
            agentEvent(a, "agent.paused", `${a.name}: paused`, {}, "operator");
            if (LIVE.includes(a.status)) {
              endSession(a, "stopped");
              setStatus(a, "stopped", "paused");
            }
          }
          return clone(a);
        }),
      // 202. At rest: the compaction starts now (`working`, `compacting`), then `agent.compacted`.
      // Busy or `defer`: `reset_pending`, and it runs when the turn ends. `then` goes as the next turn.
      compact: (id, body = {}) =>
        reply(() => {
          const a = agent(id);
          const then = resetThenOf(body.then);
          if (!MOCK_COMPACT_HARNESSES.includes(a.harness)) fail(409, "conflict", `auto compact not supported for ${a.harness}`);
          if (a.paused) fail(409, "conflict", `agent \`${a.project}/${a.name}\` is paused; it does not compact`);
          if (!LIVE.includes(a.status)) fail(409, "not_running", `agent ${a.id} is not running`);
          if (w.holds.some((h) => h.agent === a.id)) fail(409, "conflict", `agent \`${a.project}/${a.name}\` has a quota or budget hold; it does not compact until the hold ends`);
          const deferred = body.defer === true;
          const r: Reset = { kind: "compact", by: "operator", then_set: then !== undefined, recap: false, deferred, requested_at: iso(), ...(then !== undefined ? { then } : {}) };
          const state: ResetState = atRest(a) && !deferred ? "started" : "queued";
          audit("agent.compact", a.id, a.project, { state, then_set: r.then_set, deferred });
          const res = acceptReset(a, r, state);
          if (state === "started") {
            runCompact(a, r);
            return { ...clone(a), reset: res.reset };
          }
          return res;
        }),
      // 202. At rest: a new session now. Busy, paused, or `defer`: `reset_pending`, and it runs when the turn ends.
      // Stopped: with `then` it starts at once in a new session; without, the next wake starts a new session.
      newSession: (id, body = {}) =>
        reply(() => {
          const a = agent(id);
          const then = resetThenOf(body.then);
          const deferred = body.defer === true;
          const live = LIVE.includes(a.status);
          const state: ResetState = a.paused ? "queued" : !live ? (then !== undefined ? "started" : "next_wake") : atRest(a) && !deferred ? "started" : "queued";
          if (state === "started" && !live) {
            const dis = disabledRefusal(a.subscription);
            if (dis) fail(409, "conflict", dis);
          }
          const r: Reset = { kind: "new_session", by: "operator", then_set: then !== undefined, recap: body.recap !== false, deferred, requested_at: iso(), ...(then !== undefined ? { then } : {}) };
          audit("agent.new_session", a.id, a.project, { state, then_set: r.then_set, recap: r.recap, deferred });
          const res = acceptReset(a, r, state);
          if (state === "started") {
            runNewSession(a, r);
            return { ...clone(a), reset: res.reset };
          }
          return res;
        }),
      // 202, idempotent. The agent wakes at once only when work waits for it.
      unpause: (id) =>
        reply(() => {
          const a = agent(id);
          if (a.paused) {
            a.paused = false;
            a.updated_at = iso();
            audit("agent.unpause", a.id, a.project);
            agentEvent(a, "agent.unpaused", `${a.name}: unpaused`, {}, "operator");
            if (a.queued) later(() => wake(a, "unpause"), 0);
          }
          return clone(a);
        }),
      send: (id, text) =>
        reply(() => {
          const a = agent(id);
          if (!text.trim()) fail(422, "invalid", "the text is empty");
          audit("agent.send", a.id, a.project, { chars: text.length });
          if (!LIVE.includes(a.status) || a.paused) {
            // The turn waits; the agent wakes (unless paused) and then takes it.
            (queuedTurns[a.id] ??= []).push({ id: ++nextQueued, source: "store", text: `[User] ${text}`, body: text, created_at: iso() });
            a.queued = (a.queued ?? 0) + 1;
            if (!a.paused) later(() => wake(a, "turn"), 0);
            return { accepted: true, queued: true };
          }
          deliverTurn(a, text);
          return { accepted: true };
        }),
      queued: (id) => reply(() => ({ items: clone(queuedTurns[agent(id).id] ?? []) })),
      removeQueued: (id, turn, source = "store") =>
        reply(() => {
          const a = agent(id);
          if (source !== "store") fail(422, "invalid", `unknown queue \`${source}\`: give \`store\` (the source of the turn in the list)`);
          const list = queuedTurns[a.id] ?? [];
          const i = list.findIndex((t) => t.id === Number(turn));
          if (i < 0) fail(404, "not_found", `turn ${turn} does not wait for the agent (it went out, or it never was)`);
          const [gone] = list.splice(i, 1);
          a.queued = (a.queued ?? 1) - 1;
          if (a.queued <= 0) delete a.queued;
          audit("agent.queued.remove", a.id, a.project, { turn: gone!.id, bytes: gone!.text.length });
        }),
      attach: (id, file) =>
        reply(() => {
          const a = agent(id);
          // The daemon checks the magic bytes; the mock trusts the type of the Blob.
          const types = { "image/png": "png", "image/jpeg": "jpg", "image/gif": "gif", "image/webp": "webp" } as const;
          const type = (file as Blob | undefined)?.type as keyof typeof types;
          if (!(type in types)) fail(415, "unsupported_media_type", `the Content-Type \`${String(type)}\` is not an image type; send image/png, image/jpeg, image/gif, or image/webp`);
          if (file.size > 20 * 1024 * 1024) fail(413, "too_large", "an image has at most 20 MiB");
          if (file.size === 0) fail(422, "invalid", "the image is empty");
          const stamp = new Date().toISOString().replace(/[-:]/g, "").replace("T", "-").slice(0, 15);
          const name = `${stamp}-${Math.random().toString(36).slice(2, 10).padEnd(8, "0")}.${types[type]}`;
          audit("agent.attachment", a.id, a.project, { name, bytes: file.size, content_type: type });
          return { path: `/work/${a.project}/${a.name}/.terrarium/attachments/${name}`, name, bytes: file.size, content_type: type };
        }),
      answer: (id) =>
        reply(() => {
          const a = agent(id);
          if (!LIVE.includes(a.status)) fail(409, "not_running", `agent \`${a.project}/${a.name}\` is not running`);
          audit("agent.answer", a.id, a.project);
          return { accepted: true };
        }),
      events: (id, q = {}) =>
        reply(() => {
          const a = agent(id);
          return eventsAfter(q, (e) => e.agent_id === a.id);
        }),
      sessions: (id, q = {}) =>
        reply(() => {
          const a = agent(id);
          const list = w.sessions.filter((s) => s.agent === a.id).sort((x, y) => y.started_at.localeCompare(x.started_at));
          const pg = paginate(list, q, (s) => s.id);
          return { ...pg, items: pg.items.map(sessionView) };
        }),
      transcript: (id, session, q = {}) =>
        reply(() => {
          const a = agent(id);
          const s = find(w.sessions, (x) => x.id === session && x.agent === a.id, `session \`${session}\``);
          const after = Math.max(0, Number(q.after ?? 0) || 0);
          const limit = Math.min(Math.max(1, Number(q.limit ?? 500) || 500), 1000);
          const all = blocksOf(s.id).filter((b) => b.seq > after);
          const blocks = all.slice(0, limit);
          return { session: s.id, harness: a.harness, active: sessionView(s).active ?? false, blocks, next_after: blocks.at(-1)?.seq ?? after, complete: all.length <= limit };
        }),
      transcriptStream: (id, session, o = {}) =>
        createTranscriptStream(async (after, signal) => {
          // Like a socket, the stream checks auth when it opens (an SSE response, not a guarded call).
          authorize("GET");
          await (latency > 0 ? new Promise((r) => setTimeout(r, latency)) : Promise.resolve());
          const a = agent(id);
          const s = find(w.sessions, (x) => x.id === session && x.agent === a.id, `session \`${session}\``);
          return transcriptBody(s.id, after, signal);
        }, { after: o.after }),
      roles: (id) => reply(() => agent(id).roles),
      setRoles: (id, roles) =>
        reply(() => {
          const a = agent(id);
          if (a.source === "file") fail(409, "file_managed", `agent \`${a.project}/${a.name}\` is declared in a definition file`);
          const next = [...new Set(roles.filter((r) => r !== "member"))];
          for (const r of next) if (r !== "project_admin" && r !== "sysadmin") fail(422, "invalid", `\`${String(r)}\` is not a role`);
          audit("agent.roles", a.id, a.project, { from: a.roles, to: next });
          a.roles = next;
          return a.roles;
        }),
      leases: (id, o = {}) =>
        reply(() => {
          const a = agent(id);
          return leasesWhere((l) => l.agent === a.id, o.all);
        }),
      waits: waitMock.agentWaits,
      watches: waitMock.agentWatches,
      watch: waitMock.agentWatch,
      sendRaw: (id, text) =>
        reply(() => {
          const a = agent(id);
          if (!text.trim()) fail(422, "invalid", "the turn text is empty");
          if (!LIVE.includes(a.status)) fail(409, "not_running", `agent ${a.id} is not running`);
          // The audit row has the length and the first word, never the text.
          audit("agent.send_raw", a.id, a.project, { bytes: text.length, first_word: text.trim().split(/\s+/)[0]!.slice(0, 40) });
          return { accepted: true, raw: true };
        }),
      keys: (id, keys) =>
        reply(() => {
          const a = agent(id);
          if (keys.length === 0) fail(422, "invalid", "send at least one key");
          if (keys.length > 20) fail(422, "invalid", "at most 20 keys in one request");
          const names = keys.map((k) => mockKeyName(k));
          if (!LIVE.includes(a.status)) fail(409, "not_running", `agent ${a.id} is not running`);
          audit("agent.keys", a.id, a.project, { keys: names });
          return { accepted: true, keys: names };
        }),
      interrupt: (id) =>
        reply(() => {
          const a = agent(id);
          if (!LIVE.includes(a.status)) fail(409, "not_running", `agent ${a.id} is not running`);
          if (a.status !== "working" && a.status !== "blocked") fail(409, "conflict", "the agent runs no turn: there is nothing to interrupt");
          // The steps of the turn stop; the harness writes its own interrupt note (Claude Code: a system block).
          turnGen[a.id] = (turnGen[a.id] ?? 0) + 1;
          writeBlock(a, { role: "system", kind: "system", text: "[Request interrupted by user]" });
          agentEvent(a, "system", `${a.name}: interrupt: the interrupt keys went to the harness; the turn stops`, { message: "interrupt: the interrupt keys went to the harness; the turn stops" });
          audit("agent.interrupt", a.id, a.project, { harness: a.harness });
          setStatus(a, "idle", "interrupted");
          return { accepted: true, interrupted: true };
        }),
      screen: (id) =>
        reply((): AgentScreen => {
          const a = agent(id);
          if (!LIVE.includes(a.status)) fail(409, "not_running", `agent \`${a.project}/${a.name}\` is not running`);
          const text = [`✻ ${a.harness}  (mock screen for ${a.name})`, `cwd: ${a.workspace}`, "", a.status === "working" ? "● Working…" : ">"].join("\n");
          return { agent: `${a.project}/${a.name}`, agent_id: a.id, status: a.status, rows: 24, cols: 80, text, cursor: { row: 3, col: 2 }, at: iso() };
        }),
      spawn: (owner, body) => spawnMock.spawn(owner, body),
      spawned: (owner, q) => spawnMock.spawned(owner, q),
      cancel: (id) => spawnMock.cancel(id),
      spawnGet: (id) => spawnMock.get(id),
      pty: (id) => {
        const a = w.agents.find((x) => x.id === id || `${x.project}/${x.name}` === id || x.name === id);
        const name = a?.name ?? id;
        let n = 0;
        return sock(`/api/agents/${encodeURIComponent(id)}/pty`, (protocol) => new FakePty({
          protocol,
          // The daemon answers 409 not_running before the upgrade.
          refuse: !a || !LIVE.includes(a.status),
          banner: [
            `\x1b[1;35m✻ ${a?.harness ?? "claude"}\x1b[0m  \x1b[2m(mock PTY for ${name})\x1b[0m`,
            `\x1b[2mcwd: ${a?.workspace ?? "/state/agents"}\x1b[0m`,
            "",
            "Type a line and press Enter. The mock echoes it.",
          ],
          ticker: () => {
            if (a?.status !== "working") return null;
            n++;
            return `\x1b[36m●\x1b[0m ${TOOLS[n % TOOLS.length]}(${FILES[n % FILES.length]}) \x1b[2m⎿ ok\x1b[0m`;
          },
          onLine: (line) => (line ? [`\x1b[33m(mock)\x1b[0m turn received: ${line}`, `\x1b[2mThinking… done.\x1b[0m`] : []),
        }));
      },
    },

    board: {
      get: (proj) => reply(() => boardOf(proj)),
      update: (proj, columns: ColumnEdit[]) =>
        reply(() => {
          project(proj);
          const next = checkColumnEdits(w.columns[proj] ?? [], columns);
          const old = w.columns[proj] ?? [];
          const kept = new Set(columns.map((c) => c.rename_from ?? c.name));
          for (const o of old) {
            if (kept.has(o.name)) continue;
            const cards = activeIn(proj, o.name).map((c) => c.id);
            if (cards.length) fail(409, "conflict", `column \`${o.name}\` has ${cards.length} card(s): ${cards.join(", ")}; move them first`, { column: o.name, cards });
          }
          for (const c of columns.filter((x) => x.rename_from && x.rename_from !== x.name))
            for (const k of w.cards.filter((x) => x.project === proj && x.column === c.rename_from)) k.column = c.name;
          w.columns[proj] = next;
          const after = next.map((c, i) => ({ name: c.name, title: c.title ?? null, wip_limit: c.wip_limit ?? null, role: c.role ?? null, rename_from: columns[i]!.rename_from ?? null }));
          audit("board.columns", proj, proj, { columns: after });
          emit({ kind: "board", actor: "operator", project: proj, summary: `${proj}: columns changed`, data: { columns: after } });
          return boardOf(proj);
        }),
    },

    cards: {
      list: (proj, q = {}) =>
        reply(() => {
          project(proj);
          const rows = w.cards
            .filter((c) => c.project === proj && (q.archived ? true : !c.archived) && (!q.column || c.column === listColumn(proj, q.column)) && (!q.label || c.labels.includes(q.label)) && (!q.unassigned || !c.assignee) && (!q.assignee || c.assignee === q.assignee || c.assignee_name === q.assignee))
            .sort((a, b) => Number(a.id.split("-")[1]) - Number(b.id.split("-")[1]))
            .map(view);
          return paginate(rows, q, (c) => c.id.split("-")[1]!);
        }),
      create: (proj, c) =>
        reply(() => {
          const p = project(proj);
          const msg = c.from_mail ? find(w.mail, (m) => m.id === c.from_mail, `message ${c.from_mail}`) : undefined;
          const title = c.title?.trim() || msg?.subject || fail(422, "invalid", "a card needs a title");
          const n = Math.max(0, ...w.cards.filter((x) => x.project === proj).map((x) => Number(x.id.split("-")[1]) || 0)) + 1;
          const col = columnOf(proj, c.column ?? "backlog");
          const column = col.name;
          if (col.role === "blocked" && !c.comment?.trim()) fail(422, "comment_required", `moving a card to \`${column}\` (the blocked column) needs a comment that says what is needed`);
          // A new card in a full column is refused too (store/board.rs create_card moves it there).
          if (column !== "backlog" && col.wip_limit !== undefined && activeIn(proj, column).length >= col.wip_limit)
            fail(409, "wip_limit", `column \`${column}\` is at its WIP limit (${col.wip_limit})`, { column, wip_limit: col.wip_limit });
          const eta = c.eta_at ? resolveEta(c.eta_at) : undefined;
          if (eta && "error" in eta) fail(422, "invalid", eta.error);
          const card: Card = {
            id: `${p.card_prefix}-${n}`,
            project: proj,
            column,
            position: activeIn(proj, column).length,
            title,
            body: c.body ?? (msg ? `From ${msg.from_name}:\n\n${msg.body}` : ""),
            labels: c.labels ?? [],
            ...(c.repo ? { repo: c.repo } : {}),
            ...(c.branch ? { branch: c.branch } : {}),
            depends_on: c.depends_on ?? [],
            blocked_by: [],
            comments: [],
            links: msg ? [{ kind: "thread", ref: msg.thread_id, created_by: "operator", created_at: iso() }] : [],
            created_by: "operator",
            created_by_name: "operator",
            created_at: iso(),
            updated_at: iso(),
            archived: false,
          };
          w.cards.push(card);
          if (c.comment?.trim()) comment(card, c.comment.trim());
          cardEvent(card, "created", `operator created ${card.id}`);
          if (c.assignee) assign(card, c.assignee);
          if (c.eta_at) setEta(card, c.eta_at);
          return view(card);
        }),
      get: (id) => reply(() => view(cardById(id))),
      update: (id, patch) =>
        reply(() => {
          const c = cardById(id);
          if (c.archived) fail(409, "conflict", `${c.id} is archived`);
          const fields: string[] = [];
          const self = patch.assignee === "me" || patch.assignee === "operator";
          if (patch.assignee !== undefined && self) assign(c, patch.assignee);
          if (patch.column !== undefined || patch.position !== undefined) move(c, patch.column ?? c.column, patch.position, patch.comment);
          else if (patch.comment?.trim()) {
            comment(c, patch.comment.trim());
            cardEvent(c, "commented", `operator commented on ${c.id}`, { comment: patch.comment.trim() });
          }
          if (patch.assignee !== undefined && !self) assign(c, patch.assignee);
          for (const k of ["title", "body", "labels", "depends_on"] as const) {
            if (patch[k] !== undefined) {
              (c as unknown as Record<string, unknown>)[k] = patch[k];
              fields.push(k);
            }
          }
          for (const k of ["repo", "branch"] as const) {
            if (patch[k] !== undefined) {
              if (patch[k] === null) delete c[k];
              else c[k] = patch[k];
              fields.push(k);
            }
          }
          if (fields.length) {
            c.updated_at = iso();
            cardEvent(c, "edited", `operator edited ${c.id}`, { fields });
          }
          if (patch.eta_at !== undefined) setEta(c, patch.eta_at);
          return view(c);
        }),
      archive: (id) =>
        reply(() => {
          const c = cardById(id);
          if (c.archived) fail(409, "conflict", `${c.id} is archived`);
          c.archived = true;
          reindex(c.project, c.column);
          cardEvent(c, "archived", `operator archived ${c.id}`);
        }),
      move: (id, to) =>
        reply(() => {
          const c = cardById(id);
          move(c, to.column, to.position, to.comment);
          return view(c);
        }),
      assign: (id, assignee) =>
        reply(() => {
          const c = cardById(id);
          assign(c, assignee);
          return view(c);
        }),
      comment: (id, body) =>
        reply(() => {
          const c = cardById(id);
          if (!body.trim()) fail(422, "invalid", "the comment is empty");
          comment(c, body);
          cardEvent(c, "commented", `operator commented on ${c.id}`, { comment: body });
          return view(c);
        }),
      link: (id, kind, ref) =>
        reply(() => {
          const c = cardById(id);
          c.links.push({ kind, ref, created_by: "operator", created_at: iso() });
          cardEvent(c, "linked", `operator linked ${kind} ${ref} to ${c.id}`, { kind, ref });
          return view(c);
        }),
      sessions: (id) => reply(() => cardSessions(cardById(id))),
      // store/board.rs active_cards: the operator reads every project; a
      // column matches when its role or its name is one of `columns`.
      active: (q = {}) =>
        reply(() => {
          const wanted = (q.columns ?? ["working", "research"]).map((x) => x.toLowerCase());
          const colIndex = (proj: string, name: string) => (w.columns[proj] ?? []).findIndex((x) => x.name === name);
          const rows: ActiveCard[] = w.cards
            .filter((c) => !c.archived)
            .filter((c) => {
              const role = roleOf(c.project, c.column);
              return (role !== undefined && wanted.includes(role)) || wanted.includes(c.column.toLowerCase());
            })
            .sort((a, b) => a.project.localeCompare(b.project) || colIndex(a.project, a.column) - colIndex(b.project, b.column) || a.position - b.position)
            .map((c) => {
              const as = c.assignee ? w.agents.find((a) => a.id === c.assignee) : undefined;
              const role = roleOf(c.project, c.column);
              const kind = role === "working" || role === "blocked" ? role : c.column.toLowerCase();
              const row: ActiveCard = { ...view(c), column_kind: kind, ...(as ? { assignee_status: as.status } : {}) };
              if (kind === "blocked") {
                // The daemon's `blocked_info`: the last move to the blocked column, else updated_at and the newest comment.
                const ev = w.events.filter((e) => e.card === c.id && e.kind === "card" && e.data.change === "moved" && e.data.to_role === "blocked").at(-1);
                const note = typeof ev?.data.comment === "string" && ev.data.comment.trim() ? ev.data.comment : row.comments.at(-1)?.body;
                row.blocked_since = ev?.at ?? c.updated_at;
                if (note) row.blocked_reason = note;
              }
              return row;
            });
          return rows.slice(0, 500);
        }),
      events: (id, q = {}) =>
        reply(() => {
          const c = cardById(id);
          // The card events have no `order` (docs/api.md §8.3): oldest first only.
          return eventsAfter({ ...q, order: undefined }, (e) => e.card === c.id);
        }),
    },

    cardFiles: cardFileMock.api,

    sessions: {
      get: (id) =>
        reply(() => {
          const r = sessionRef(id) ?? fail(404, "not_found", `no session \`${id}\``);
          const s = w.sessions.find((x) => x.id === id)!;
          return { ...r, ...(s.harness_session_id ? { harness_session_id: s.harness_session_id } : {}), active: !r.removed && (sessionView(s).active ?? false) };
        }),
      transcript: (id, q = {}) =>
        reply(() => {
          const r = sessionRef(id) ?? fail(404, "not_found", `no session \`${id}\``);
          const s = w.sessions.find((x) => x.id === id)!;
          const after = Math.max(0, Number(q.after ?? 0) || 0);
          const limit = Math.min(Math.max(1, Number(q.limit ?? 500) || 500), 1000);
          const all = blocksOf(s.id).filter((b) => b.seq > after);
          const blocks = all.slice(0, limit);
          return { session: s.id, harness: r.harness, active: !r.removed && (sessionView(s).active ?? false), blocks, next_after: blocks.at(-1)?.seq ?? after, complete: all.length <= limit };
        }),
    },

    memories: {
      list: (proj, q = {}) =>
        reply(() => {
          project(proj);
          const needle = q.q?.toLowerCase();
          const authorAddr = q.author === "me" ? "operator" : q.author;
          return w.memories
            .filter(
              (m) =>
                m.project === proj &&
                (q.all || !m.superseded_by) &&
                (!q.tag || m.tags.includes(q.tag)) &&
                (!authorAddr || m.author === authorAddr || m.author_name === authorAddr) &&
                (!needle || needle.split(/\s+/).every((word) => `${m.title} ${m.body} ${m.tags.join(" ")}`.toLowerCase().includes(word))),
            )
            .sort((x, y) => memNo(y.id) - memNo(x.id))
            .map(memView);
        }),
      create: (proj, m) =>
        reply(() => {
          project(proj);
          if (!m.title?.trim()) fail(422, "invalid", "a memory needs a title");
          const n = Math.max(0, ...w.memories.filter((x) => x.project === proj).map((x) => memNo(x.id))) + 1;
          const id = `MEM-${n}`;
          const words = m.title.toLowerCase().split(/\s+/);
          const similar = w.memories
            .filter((x) => x.project === proj && words.some((wd) => x.title.toLowerCase().includes(wd)))
            .slice(0, 5)
            .map((x) => ({ id: x.id, title: x.title, superseded: !!x.superseded_by }));
          const mm: Memory = { id, key: `${proj}/${id}`, project: proj, title: m.title, body: m.body ?? "", tags: m.tags ?? [], author: "operator", author_name: "operator", sources: m.sources ?? [], supersedes: [], links: [], created_at: iso(), updated_at: iso() };
          w.memories.push(mm);
          for (const l of m.links ?? []) w.links.push({ from: id, to: memory(l.to, proj).id, relation: l.relation, note: l.note, author: "operator", author_name: "operator", created_at: iso() });
          emit({ kind: "memory.created", actor: "operator", project: proj, summary: `operator wrote ${id}: ${m.title}`, data: { memory_id: id, key: mm.key, title: m.title } });
          return { ...memView(mm), similar };
        }),
      get: (id) => reply(() => memView(memory(id))),
      update: (id, patch) =>
        reply(() => {
          const m = memory(id);
          const fields: string[] = [];
          if (patch.title !== undefined) m.title = patch.title;
          if (patch.body !== undefined) m.body = patch.body;
          if (patch.sources !== undefined) m.sources = patch.sources;
          if (patch.tags !== undefined) m.tags = patch.tags;
          if (patch.add_tags) m.tags = [...new Set([...m.tags, ...patch.add_tags])];
          if (patch.remove_tags) m.tags = m.tags.filter((t) => !patch.remove_tags!.includes(t));
          for (const k of ["title", "body", "sources", "tags"] as const) if (patch[k] !== undefined) fields.push(k);
          if (patch.add_tags || patch.remove_tags) fields.push("tags");
          m.updated_at = iso();
          emit({ kind: "memory.updated", actor: "operator", project: m.project, summary: `operator edited ${m.id}`, data: { memory_id: m.id, key: m.key, title: m.title, fields } });
          return memView(m);
        }),
      remove: (id) =>
        reply(() => {
          const m = memory(id);
          w.memories = w.memories.filter((x) => x !== m);
          w.links = w.links.filter((l) => !(memKey(m.project, l.from) === m.key || memKey(m.project, l.to) === m.key));
          audit("memory.delete", m.key, m.project, { title: m.title });
          emit({ kind: "memory.deleted", actor: "operator", project: m.project, summary: `operator deleted ${m.id}`, data: { memory_id: m.id, key: m.key, title: m.title } });
        }),
      graph: (id, depth = 1) =>
        reply(() => {
          const root = memory(id);
          const d = Math.max(1, Math.min(depth, 5));
          const proj = root.project;
          const seen = new Set([root.id]);
          let frontier = [root.id];
          for (let i = 0; i < d; i++) {
            const next: string[] = [];
            for (const l of w.links.filter((x) => memProject(x.from) === proj)) {
              for (const [x, y] of [[l.from, l.to], [l.to, l.from]] as const) {
                if (frontier.includes(x) && !seen.has(y)) {
                  seen.add(y);
                  next.push(y);
                }
              }
            }
            frontier = next;
          }
          return {
            root: root.id,
            depth: d,
            nodes: w.memories.filter((m) => m.project === proj && seen.has(m.id)).map(memView),
            edges: w.links.filter((l) => memProject(l.from) === proj && seen.has(l.from) && seen.has(l.to)).map(({ from, to, relation, note, author, created_at }) => ({ from, to, relation, ...(note ? { note } : {}), author, created_at })),
          };
        }),
      link: (id, to, relation, note) =>
        reply(() => {
          const m = memory(id);
          const other = memory(to, m.project);
          if (other.project !== m.project) fail(422, "invalid", "links stay inside one project");
          if (relation === "supersedes") supersede(other, m);
          else w.links.push({ from: m.id, to: other.id, relation, ...(note ? { note } : {}), author: "operator", author_name: "operator", created_at: iso() });
          emit({ kind: "memory.linked", actor: "operator", project: m.project, summary: `operator linked ${m.id} ${relation} ${other.id}`, data: { memory_id: m.id, key: m.key, title: m.title, to: other.id, relation, note: note ?? null } });
          return memView(m);
        }),
      unlink: (id, to, relation) =>
        reply(() => {
          const m = memory(id);
          const other = memory(to, m.project);
          w.links = w.links.filter((l) => !(l.from === m.id && l.to === other.id && l.relation === relation && memProject(l.from) === m.project));
          emit({ kind: "memory.unlinked", actor: "operator", project: m.project, summary: `operator unlinked ${m.id} ${relation} ${other.id}`, data: { memory_id: m.id, key: m.key, title: m.title, to: other.id, relation } });
          return memView(m);
        }),
      supersede: (id, by) =>
        reply(() => {
          const old = memory(id);
          const next = memory(by, old.project);
          supersede(old, next);
          return memView(old);
        }),
    },

    wiki: {
      tree: (proj, q = {}) =>
        reply(() => {
          project(proj);
          ensureIndex(proj);
          const needle = q.q?.toLowerCase();
          return w.pages
            .filter((p) => p.project === proj && (q.deleted ? !!p.deleted_at : !p.deleted_at) && (!q.status || p.status === q.status) && (!needle || `${p.path} ${p.title} ${p.body}`.toLowerCase().includes(needle)))
            .sort((x, y) => x.path.localeCompare(y.path))
            .map((p) => summaryOf(p, needle));
        }),
      create: (proj, pg) =>
        reply(() => {
          project(proj);
          if (!/^[a-z0-9_.-]+(\/[a-z0-9_.-]+)*$/.test(pg.path)) fail(422, "invalid", `\`${pg.path}\` is not a valid page path`);
          if (w.pages.some((p) => p.project === proj && p.path === pg.path && !p.deleted_at)) fail(409, "conflict", `page \`${pg.path}\` exists`);
          if (!pg.summary.trim()) fail(422, "invalid", "a revision needs a summary");
          const created: WikiPage = { id: ulid("pg"), project: proj, path: pg.path, title: "", rev: 1, body: pg.body, author: "operator", author_name: "operator", summary: pg.summary, created_by: "operator", created_at: iso(), updated_at: iso(), links: [] };
          applyBody(created, pg.body, pg.title);
          w.pages.push(created);
          w.revisions[created.id] = [{ page: created.id, rev: 1, author: "operator", author_name: "operator", summary: pg.summary, created_at: iso(), body: pg.body }];
          wikiEvent("wiki.created", created, `wiki ${created.path} created (rev 1)`);
          return created;
        }),
      get: (id) => reply(() => page(id)),
      save: (id, body) =>
        reply(() => {
          const pg = page(id);
          if (body.base !== pg.rev) {
            const baseBody = (w.revisions[pg.id] ?? []).find((r) => r.rev === body.base)?.body ?? "";
            fail(409, "stale_base", `page \`${pg.path}\` is at rev ${pg.rev}, not your base rev ${body.base}; merge the changes since your base, then save with base ${pg.rev}`, {
              base: body.base,
              current: pg.rev,
              diff: `--- rev ${body.base}\n+++ rev ${pg.rev}\n${lineDiff(baseBody, pg.body)}`,
            });
          }
          if (!body.summary.trim()) fail(422, "invalid", "a revision needs a summary");
          newRevision(pg, body.body, body.summary, body.title);
          wikiEvent("wiki.saved", pg, `wiki ${pg.path} saved (rev ${pg.rev})`, { summary: body.summary });
          return pg;
        }),
      remove: (id) =>
        reply(() => {
          const pg = page(id);
          pg.deleted_at = iso();
          audit("wiki.delete", pg.id, pg.project, { path: pg.path });
          wikiEvent("wiki.deleted", pg, `wiki ${pg.path} deleted`);
        }),
      section: (id, sct) =>
        reply(() => {
          const pg = page(id);
          if (!sct.summary.trim()) fail(422, "invalid", "a revision needs a summary");
          newRevision(pg, `${pg.body.replace(/\n*$/, "")}\n\n## ${sct.heading}\n\n${sct.body}\n`, sct.summary);
          wikiEvent("wiki.saved", pg, `wiki ${pg.path} saved (rev ${pg.rev})`, { summary: sct.summary });
          return pg;
        }),
      append: (id, sct) =>
        reply(() => {
          const pg = page(id);
          if (!sct.summary.trim()) fail(422, "invalid", "a revision needs a summary");
          newRevision(pg, `${pg.body.replace(/\n*$/, "")}\n\n${sct.body}\n`, sct.summary);
          wikiEvent("wiki.saved", pg, `wiki ${pg.path} saved (rev ${pg.rev})`, { summary: sct.summary });
          return pg;
        }),
      revisions: (id) => reply(() => (w.revisions[page(id).id] ?? []).map(({ body: _b, ...r }) => r).reverse()),
      revision: (id, rev) => reply(() => find(w.revisions[page(id).id] ?? [], (r) => r.rev === rev, `revision ${rev}`)),
      diff: (id, x, y) =>
        reply(() => {
          const pg = page(id);
          const b = y ?? pg.rev;
          const a = x ?? b - 1;
          const ra = find(w.revisions[pg.id] ?? [], (r) => r.rev === a, `revision ${a}`).body;
          const rb = find(w.revisions[pg.id] ?? [], (r) => r.rev === b, `revision ${b}`).body;
          return { page: pg.id, a, b, diff: `--- rev ${a}\n+++ rev ${b}\n${lineDiff(ra, rb)}` };
        }),
      restore: (id, rev) =>
        reply(() => {
          const pg = page(id);
          const r = find(w.revisions[pg.id] ?? [], (x) => x.rev === rev, `revision ${rev}`);
          const from = pg.rev;
          newRevision(pg, r.body, `Restore rev ${rev}`);
          wikiEvent("wiki.restored", pg, `wiki ${pg.path} restored rev ${rev}`, { from_rev: from });
          return pg;
        }),
      move: (id, path) =>
        reply(() => {
          const pg = page(id);
          const from = pg.path;
          pg.path = path;
          audit("wiki.move", pg.id, pg.project, { from, to: path });
          wikiEvent("wiki.moved", pg, `wiki ${from} moved to ${path}`, { from });
          return pg;
        }),
      backlinks: (id) =>
        reply(() => {
          const pg = page(id);
          return w.pages.filter((p) => p.id !== pg.id && !p.deleted_at && p.body.includes(`[[${pg.path}`)).map((p) => summaryOf(p));
        }),
      review: (id, decision, comment) =>
        reply(() => {
          const pg = page(id);
          if (decision === "comment" && !comment?.trim()) fail(422, "comment_required", "a review comment needs `comment`");
          if (decision === "approve") {
            const { body } = splitFrontMatter(pg.body);
            const fm = pg.reviewer ? `---\nstatus: reviewed\nreviewer: ${pg.reviewer}\n---\n` : "---\nstatus: reviewed\n---\n";
            newRevision(pg, fm + body, `Reviewed by operator${comment ? `: ${comment}` : ""}`);
          }
          sendMail(pg.author === "system" ? "operator" : pg.author, `project:${pg.project}`, `Review of ${pg.path}: ${decision}`, comment ?? "");
          wikiEvent("wiki.reviewed", pg, `operator ${decision === "approve" ? "approved" : "commented on"} ${pg.path}`, { decision, comment: comment ?? null });
          return pg;
        }),
    },

    artifacts: {
      list: (proj) => reply(() => (project(proj), w.artifacts.filter((a) => a.project === proj).sort((x, y) => artNo(y.id) - artNo(x.id)).map(artView))),
      usage: (proj) =>
        reply(() => {
          project(proj);
          const list = w.artifacts.filter((a) => a.project === proj);
          return { project: proj, count: list.length, used_bytes: list.reduce((n, a) => n + a.size, 0), quota_bytes: 1 << 30, max_file_bytes: 32 << 20 };
        }),
      upload: (proj, form) =>
        reply(() => {
          const file = form.get("file");
          const name = String(form.get("name") || (file instanceof File ? file.name : "upload.bin"));
          const kind = form.get("kind") === "bundle" ? ("bundle" as const) : ("file" as const);
          const ak = form.get("attach_kind");
          const ar = form.get("attach_ref");
          const attach = typeof ak === "string" && typeof ar === "string" && ak && ar ? { kind: ak as ArtifactLinkKind, ref: ar } : undefined;
          const type = kind === "bundle" ? "text/html; charset=utf-8" : file instanceof File && file.type ? file.type : "application/octet-stream";
          return artView(addArtifact(proj, name, type, file instanceof File ? file.size : 0, kind, attach));
        }),
      uploadJson: (proj, body) =>
        reply(() => {
          if (!body.name.trim()) fail(422, "invalid", "an artifact needs a name");
          const kind = body.kind ?? "file";
          return artView(addArtifact(proj, body.name, kind === "bundle" ? "text/html; charset=utf-8" : (body.content_type ?? "application/octet-stream"), Math.floor((body.data_base64.length * 3) / 4), kind, body.attach));
        }),
      get: (id) => reply(() => artView(artifact(id))),
      remove: (id) =>
        reply(() => {
          const a = artifact(id);
          w.artifacts = w.artifacts.filter((x) => x !== a);
          emit({ kind: "artifact.deleted", actor: "operator", project: a.project, summary: `operator deleted ${a.id}`, data: { artifact_id: a.id, key: a.key, uid: a.uid, name: a.name, kind: a.kind } });
        }),
      link: (id, kind, ref) =>
        reply(() => {
          const a = artifact(id);
          const l = linkOf(kind, ref);
          const label = l.kind === "page" ? w.pages.find((p) => p.id === ref || p.path === ref)?.path : undefined;
          if (!a.links.some((x) => x.kind === l.kind && x.ref === l.ref)) a.links.push({ ...l, ...(label ? { label } : {}), created_by: "operator", created_at: iso() });
          emit({ kind: "artifact.linked", actor: "operator", project: a.project, summary: `operator attached ${a.id} to ${kind} ${ref}`, data: { artifact_id: a.id, key: a.key, uid: a.uid, name: a.name, kind: a.kind, link_kind: kind, ref } });
          return artView(a);
        }),
      // The daemon signs `/a/{uid}/{name}?exp&sig`. The mock hands out data: URLs.
      url: (id) =>
        reply(() => {
          const a = artifact(id);
          const raw = w.artifactBodies[a.id] ?? `uploaded ${a.name}`;
          const url = raw.startsWith("data:") ? raw : `data:${a.content_type.split(";")[0]};charset=utf-8,${encodeURIComponent(raw)}`;
          const exp = Math.floor(now() / 1000) + 300;
          return { url, exp, expires_at: new Date(exp * 1000).toISOString() };
        }),
      text: (url) =>
        reply(() => {
          const m = /^data:[^,]*,(.*)$/s.exec(url);
          return m ? decodeURIComponent(m[1]!) : "";
        }),
    },

    subscriptions: {
      list: () => reply(() => w.subscriptions.map(subView)),
      get: (name) => reply(() => subView(sub(name))),
      create: (def) =>
        reply(() => {
          checkSubName(def.name);
          if (w.subscriptions.some((x) => x.name === def.name)) fail(409, "conflict", `subscription \`${def.name}\` exists`);
          if (def.probe && !PROBES.includes(def.probe)) fail(422, "invalid", `unknown probe \`${def.probe}\``);
          if (!def.provider?.trim()) fail(422, "invalid", "the provider is empty");
          if (isZai(def.provider)) { const p = zaiPlan(def.plan); if ("error" in p) fail(422, "invalid", p.error); }
          // Moonshot is for pi only; an Open Platform plan gets the balance probe.
          let probe = def.probe;
          if (selfhosted.isSelfHosted(def.provider)) {
            if (def.probe) fail(422, "invalid", `subscription \`${def.name}\`: a self-hosted server has no quota probe`);
            if (!def.selfhosted) fail(422, "invalid", `subscription \`${def.name}\`: a self-hosted subscription needs \`selfhosted.base_url\``);
            const alias = def.provider.trim().toLowerCase() === selfhosted.OPENAI_COMPATIBLE;
            const c = selfhosted.checkSelfHosted({ ...def.selfhosted!, ...(alias ? { api: "openai" as const } : {}) });
            if ("error" in c) fail(422, "invalid", `subscription \`${def.name}\`: ${c.error}`);
            const sh = (c as { sh: NonNullable<Subscription["selfhosted"]> }).sh;
            const h = selfhosted.selfHostedHarnesses(def.name, sh.api, def.harnesses);
            if ("error" in h) fail(422, "invalid", h.error);
            const s: Subscription = {
              name: def.name, provider: selfhosted.SELFHOSTED, harnesses: (h as { harnesses: Agent["harness"][] }).harnesses, alert_at: def.alert_at ?? 80, source: "api",
              login_status: "unknown", probe_status: "none", created_at: iso(), updated_at: iso(), windows: [], local_estimate_only: true, has_api_key: false, held_agents: 0, agents: [],
              selfhosted: sh, enabled: true,
            };
            w.subscriptions.push(s);
            audit("subscription.create", s.name, undefined, { provider: s.provider, selfhosted: sh });
            return subView(s);
          }
          if (def.selfhosted) fail(422, "invalid", `subscription \`${def.name}\`: \`selfhosted\` is only for the provider selfhosted, not \`${def.provider}\``);
          if (isMoonshot(def.provider)) {
            const p = moonshotPlan(def.plan);
            if ("error" in p) fail(422, "invalid", p.error);
            const h = moonshotHarnessError(def.name, def.harnesses ?? []);
            if (h) fail(422, "invalid", h);
            probe = probe ?? moonshotProbe(def.plan);
          }
          const s: Subscription = {
            name: def.name, provider: def.provider, ...(def.plan ? { plan: def.plan } : {}), harnesses: def.harnesses ?? [], ...(probe ? { probe } : {}),
            alert_at: def.alert_at ?? 80, source: "api", login_status: "unknown", probe_status: "none", created_at: iso(), updated_at: iso(),
            windows: [], local_estimate_only: !probe, has_api_key: false, held_agents: 0, agents: [],
          };
          w.subscriptions.push(s);
          audit("subscription.create", s.name, undefined, { provider: s.provider });
          return subView(s);
        }),
      remove: (name) =>
        reply(() => {
          const s = sub(name);
          if (s.source === "file") fail(409, "file_managed", `subscription \`${name}\` comes from subscriptions/${name}.toml; edit or delete the file`);
          refuseLiveAccount(s, "delete it");
          clearDefault(s);
          w.subscriptions.splice(w.subscriptions.indexOf(s), 1);
          delete w.apiKeys[name];
          w.quota = w.quota.filter((q) => q.subscription !== name);
          audit("subscription.delete", name, undefined);
        }),
      quota: (name, q = {}) =>
        reply(() => {
          sub(name);
          const since = q.since || new Date(now() - 24 * 3600_000).toISOString();
          return w.quota.filter((x) => x.subscription === name && x.at >= since && (!q.window || x.window === q.window)).slice(0, q.limit ?? 1000);
        }),
      models: (name) =>
        reply(() => {
          const s = sub(name);
          if (selfhosted.isSelfHosted(s.provider)) {
            const sh = s.selfhosted;
            const ids = [...new Set([...(sh?.default_model ? [sh.default_model] : []), ...(sh?.models ?? [])])];
            return { models: ids.map((id) => ({ id, ...(sh?.context_window ? { context: sh.context_window } : {}) })), source: "subscription" as const, fetched_at: iso() };
          }
          return modelList(s, !!w.apiKeys[s.name], iso());
        }),
      probe: (name) =>
        reply(() => {
          const s = sub(name);
          if (!s.probe) return { subscription: s.name, probe_status: "none" as const, windows: [] };
          s.last_probe_at = iso();
          if (s.login_status === "missing" || s.login_status === "unknown" || s.probe_status === "failed") {
            s.probe_status = "failed";
            s.probe_error ??= "no credential in the shared harness home";
            return { subscription: s.name, probe_status: "failed" as const, error: s.probe_error, windows: [] };
          }
          reading(s);
          return { subscription: s.name, probe_status: "ok" as const, windows: s.windows.map((x) => ({ window: x.window, used_pct: x.used_pct, resets_at: x.resets_at, raw: { utilization: x.used_pct } })) };
        }),
      verify: (name) =>
        reply(() => {
          const s = sub(name);
          if (selfhosted.isSelfHosted(s.provider)) {
            s.login_status = "api_key";
            return { subscription: subView(s), status: { logged_in: true, detail: "a self-hosted server: no login, no stored key" } };
          }
          if (s.env_var && w.apiKeys[name]) {
            s.login_status = isZai(s.provider) || isMoonshot(s.provider) ? "api_key" : "logged_in";
            return { subscription: subView(s), status: { logged_in: true, detail: "a stored API key" } };
          }
          const harness = s.harnesses[0];
          if (!harness) fail(422, "invalid", `subscription \`${name}\` names no harness to check`);
          const ok = s.login_status !== "missing" && s.login_status !== "expired";
          s.login_status = ok ? "logged_in" : "missing";
          s.verified_at = iso();
          audit("subscription.verify", name, undefined, { logged_in: ok });
          return { subscription: subView(s), status: { kind: harness, harness, provider: s.provider, logged_in: ok, ...(s.plan ? { plan: s.plan } : {}), detail: ok ? "Logged in" : "Not logged in" } };
        }),
      logout: (name) =>
        reply(() => {
          const s = sub(name);
          const harness = s.harnesses[0];
          if (!harness) fail(422, "invalid", `subscription \`${name}\` names no harness`);
          if (harness === "pi") fail(422, "invalid", "pi has no logout command");
          if (isZai(s.provider)) fail(422, "invalid", `subscription \`${name}\` is a Z.ai API key and has no logout; store another key, or delete the subscription`);
          if (isMoonshot(s.provider)) fail(422, "invalid", `subscription \`${name}\` is a Moonshot API key and has no logout; store another key, or delete the subscription`);
          refuseLiveAccount(s, "log it out");
          s.login_status = "missing";
          if (s.account) s.account = { ...s.account, has_login: false, updated_at: iso() };
          const cleared = clearDefault(s);
          audit("subscription.logout", name, undefined, { kind: harness, default_cleared: cleared });
          return subView(s);
        }),
      makeDefault: (name) =>
        reply(() => {
          const s = sub(name);
          if (s.env_var) fail(422, "invalid", `subscription \`${name}\` is an API key; only a plan account can be the default`);
          if (!s.account) fail(422, "invalid", `subscription \`${name}\` has no plan account; log in with the wizard first`);
          const harness = s.account!.harness;
          if (!DEFAULT_HARNESSES.includes(harness)) fail(422, "invalid", `subscription \`${name}\` is not a plan account of Claude Code, Codex, or Copilot (harness ${harness}, provider ${s.provider}); only those have a default account`);
          setDefault(s, harness, "set");
          return subView(s);
        }),
      setApiKey: (name, input) =>
        reply(() => {
          if (!input.key.trim()) fail(422, "invalid", "the key is empty");
          const env = KEY_ENV[input.provider];
          if (!env) fail(422, "invalid", `unknown provider \`${input.provider}\`; use anthropic, openai, openai-compatible, google, openrouter, xai, groq, github, zai, or moonshot`);
          const planError = checkKeyPlan(input);
          if (planError) fail(422, "invalid", planError);
          const ms = isMoonshot(input.provider) ? moonshotKeyDefaults(input) : null;
          const zai = isZai(input.provider) ? zaiKeyDefaults(input) : ms;
          let s = w.subscriptions.find((x) => x.name === name);
          // A self-hosted key goes on a self-hosted subscription only; a new one needs the base URL.
          const sh = selfhosted.isSelfHosted(input.provider);
          if (s && selfhosted.isSelfHosted(s.provider) !== sh) fail(422, "invalid", `cannot store a ${input.provider} key on subscription \`${name}\`: it is a ${s.provider} subscription`);
          if (!s && sh) {
            if (!input.base_url?.trim()) fail(422, "invalid", `provider \`${input.provider}\` needs base_url for a new subscription (or create it first with POST /api/subscriptions and \`selfhosted\`)`);
            checkSubName(name);
            const c = selfhosted.checkSelfHosted({ api: "openai", base_url: input.base_url!, models: input.models });
            if ("error" in c) fail(422, "invalid", `subscription \`${name}\`: ${c.error}`);
            s = { name, provider: selfhosted.SELFHOSTED, harnesses: ["pi", "codex"], alert_at: 80, source: "api", login_status: "api_key", probe_status: "none", created_at: iso(), updated_at: iso(), windows: [], local_estimate_only: true, has_api_key: true, held_agents: 0, agents: [], selfhosted: (c as { sh: NonNullable<Subscription["selfhosted"]> }).sh, enabled: true };
            w.subscriptions.push(s);
          }
          // A Moonshot key never goes to a harness other than pi.
          if (ms && s) {
            const why = isMoonshot(s.provider) ? moonshotHarnessError(name, s.harnesses) : `it is a ${s.provider} subscription`;
            if (why) fail(422, "invalid", `cannot store a moonshot key on subscription \`${name}\`: ${why}`);
          }
          if (!s) {
            checkSubName(name);
            s = { name, provider: input.provider, plan: zai?.plan ?? "API key", harnesses: zai?.harnesses ?? [input.provider === "github" ? "copilot" : "pi"], ...(ms?.probe ? { probe: ms.probe } : {}), alert_at: 80, source: "api", login_status: "logged_in", probe_status: "none", created_at: iso(), updated_at: iso(), windows: [], local_estimate_only: true, has_api_key: true, held_agents: 0, agents: [] };
            w.subscriptions.push(s);
          }
          w.apiKeys[name] = { provider: input.provider, key: input.key, base_url: input.base_url, models: input.models };
          if (zai && s.source === "api") s.plan = zai.plan;
          if (ms && s.source === "api") { if (ms.probe) s.probe = ms.probe; else delete s.probe; }
          s.env_var = env;
          s.login_status = sh ? "api_key" : zai?.login_status ?? "logged_in";
          s.updated_at = iso();
          audit("subscription.api_key", name, undefined, { provider: input.provider, env_var: env, base_url: input.base_url ?? null, models: input.models ?? [] });
          return subView(s);
        }),
      update: (name, patch) =>
        reply(() => {
          const s = sub(name);
          if (s.source === "file") fail(409, "file_managed", `subscription \`${name}\` is declared in subscriptions/${name}.toml; change the file, then POST /api/reload`);
          if (patch.alert_at !== undefined && (patch.alert_at < 1 || patch.alert_at > 100)) fail(422, "invalid", "alert_at is a percent, 1 to 100");
          const isSh = selfhosted.isSelfHosted(s.provider);
          if ((patch.selfhosted || patch.harnesses) && !isSh) {
            if (patch.selfhosted) fail(422, "invalid", `subscription \`${name}\` is ${s.provider}, not selfhosted`);
            fail(422, "invalid", "`harnesses` changes here only for a self-hosted subscription; change the definition of another one");
          }
          let next = s.selfhosted;
          let harnesses = s.harnesses;
          if (isSh && (patch.selfhosted || patch.harnesses)) {
            const merged: Record<string, unknown> = { ...(s.selfhosted ?? { api: "openai", base_url: "" }) };
            for (const [k, v] of Object.entries(patch.selfhosted ?? {})) {
              if (v === null) delete merged[k];
              else if (v !== undefined) merged[k] = v;
            }
            const c = selfhosted.checkSelfHosted(merged as unknown as NonNullable<Subscription["selfhosted"]>);
            if ("error" in c) fail(422, "invalid", `subscription \`${name}\`: ${c.error}`);
            next = (c as { sh: NonNullable<Subscription["selfhosted"]> }).sh;
            const want = patch.harnesses ?? s.harnesses.filter((h) => selfhosted.STYLE_HARNESSES[next!.api].includes(h));
            const h = selfhosted.selfHostedHarnesses(name, next.api, want);
            if ("error" in h) fail(422, "invalid", h.error);
            harnesses = (h as { harnesses: Agent["harness"][] }).harnesses;
          }
          if (patch.alert_at !== undefined) s.alert_at = patch.alert_at;
          s.selfhosted = next;
          s.harnesses = harnesses;
          s.updated_at = iso();
          audit("subscription.update", name, undefined, { alert_at: s.alert_at, harnesses: s.harnesses, selfhosted: s.selfhosted ?? null });
          return subView(s);
        }),
      enable: (name) => reply(() => subView(setSubEnabled(sub(name), true))),
      disable: (name) => reply(() => subView(setSubEnabled(sub(name), false))),
      stopAll: (name, opts = {}) =>
        reply(() => {
          const s = sub(name);
          if (opts.disable) setSubEnabled(s, false);
          const stopped = w.agents
            .filter((a) => a.subscription === name && LIVE.includes(a.status))
            .map((a) => {
              later(() => {
                endSession(a, "stopped");
                setStatus(a, "stopped", "stopped");
              }, 0);
              return { id: a.id, name: a.name, project: a.project };
            });
          if (s.streams) s.streams = { ...s.streams, in_use: 0, waiting: 0 };
          audit("subscription.stop_all", name, undefined, { agents: stopped, disabled: !!opts.disable });
          emit({ kind: "subscription.stop_all", actor: "operator", summary: `${name}: stop all (${stopped.length} agents)`, data: { subscription: name, agents: stopped, disabled: !!opts.disable } });
          return { subscription: subView(s), stopped };
        }),
      fetchModels: (input) =>
        reply(() => {
          const stored = input.subscription ? sub(input.subscription) : undefined;
          if (stored && !selfhosted.isSelfHosted(stored.provider)) fail(422, "invalid", `subscription \`${stored.name}\` is not a self-hosted server`);
          const r = selfhosted.fetchModels(input, stored?.selfhosted, iso());
          if ("error" in r) fail(r.status, r.code, r.error);
          return (r as { result: import("../types").FetchModelsResult }).result;
        }),
      detect: () => reply(detected),
      startLogin: (req) =>
        reply(() => {
          if (!["claude", "codex", "copilot", "pi"].includes(req.kind)) fail(422, "invalid", `unknown login kind \`${req.kind}\``);
          if (req.kind === "pi") fail(422, "invalid", `${PI_PLAN_REFUSAL}; store a pi API key with POST /api/subscriptions/{name}/api-key`);
          if (req.name) checkSubName(req.name);
          if ([...logins.values()].filter((x) => !x.done).length >= 4) fail(409, "conflict", "four login sessions are live; finish or cancel one");
          const s: LoginSession & { name?: string } = {
            id: ulid("login"), kind: req.kind, ...(req.provider ? { provider: req.provider } : {}), state: "starting", started_at: iso(), done: false, instructions: INSTRUCTIONS(req.kind, req.provider),
            ...(req.name ? { name: req.name } : {}),
          };
          logins.set(s.id, s);
          audit("login.start", s.id, undefined, { kind: s.kind, provider: s.provider ?? null });
          setTimeout(() => toWaiting(s), opts.loginStepMs ?? (latency > 0 ? 800 : 0));
          setTimeout(() => endLogin(s, "expired"), opts.loginTtlMs ?? 10 * 60_000);
          return s;
        }),
      loginState: (id) => reply(() => logins.get(id) ?? fail(404, "not_found", `login session \`${id}\` not found`)),
      cancelLogin: (id) =>
        reply(() => {
          const s = live(id);
          audit("login.cancel", s.id, undefined);
          endLogin(s, "cancelled");
        }),
      loginInput: (id, code) =>
        reply(() => {
          const s = live(id);
          if (!code.trim() || /[\r\n]/.test(code)) fail(422, "invalid", "the code is empty or has a line break");
          audit("login.input", s.id, undefined, { length: code.length });
          finishLogin(s);
        }),
      loginPty: (id) => sock(`/api/subscriptions/login/${encodeURIComponent(id)}/pty`, (protocol) => fakeLoginPty(logins.get(id), protocol)),
    },

    usage: {
      series: (q) => reply(() => usageSeries(q)),
      card: (id, q = {}) =>
        reply(() => {
          const c = find(w.cards, (x) => x.id === id.trim().toUpperCase(), `card \`${id}\``);
          const since = checkTime("since", q.since);
          const until = checkTime("until", q.until);
          const rows = w.usage.filter((r) => r.card === c.id && (!since || r.at >= since) && (!until || r.at < until));
          const spend = (t: UsageTotals) => t.input_tokens + t.output_tokens;
          const split = <K extends "model" | "agent">(col: K) => {
            const m = new Map<string, UsageRow[]>();
            for (const r of rows) m.set(r[col], [...(m.get(r[col]) ?? []), r]);
            return [...m.entries()].map(([k, rs]) => [k, sumRows(rs)] as const).sort(([ka, a], [kb, b]) => spend(b) - spend(a) || ka.localeCompare(kb));
          };
          return {
            card: c.id,
            title: c.title,
            project: c.project,
            ...(since ? { since } : {}),
            ...(until ? { until } : {}),
            total: sumRows(rows),
            models: split("model").map(([model, t]) => ({ model, ...t })),
            agents: split("agent").map(([id, t]) => {
              const a = w.agents.find((x) => x.id === id);
              return { id, name: a ? `${a.project}/${a.name}` : id, ...t };
            }),
          };
        }),
      me: (key) =>
        reply(() => {
          const a = agent(key);
          const s = a.subscription ? w.subscriptions.find((x) => x.name === a.subscription) : undefined;
          const since = `${new Date(now()).toISOString().slice(0, 10)}T00:00:00Z`;
          const mine = sumRows(w.usage.filter((r) => r.agent === a.id && r.at >= since));
          const all = s ? sumRows(w.usage.filter((r) => r.subscription === s.name && r.at >= since)) : undefined;
          const whole = all ? all.input_tokens + all.output_tokens : 0;
          return {
            agent: { id: a.id, name: a.name, project: a.project },
            ...(s ? { subscription: { name: s.name, provider: s.provider, plan: s.plan, probe_status: s.probe_status, last_success_at: s.last_success_at, local_estimate_only: !s.probe, windows: s.windows } } : {}),
            today: { since, mine, ...(all ? { subscription: all } : {}), ...(whole > 0 ? { share_pct: Math.round(((mine.input_tokens + mine.output_tokens) / whole) * 1000) / 10 } : {}) },
            holds: w.holds.filter((h) => h.agent === a.id),
          };
        }),
    },

    mail: {
      list: (q = {}) =>
        reply(() => {
          // The daemon resolves `to` and `from` as addresses, and `system` is not one it resolves (store/mail.rs `resolve`).
          for (const v of [q.to, q.from]) if (v?.trim() === "system") notFound("agent `system`");
          if (q.project) project(q.project);
          for (const v of [q.since, q.until]) if (v && Number.isNaN(Date.parse(v))) fail(400, "bad_request", `bad time \`${v}\`: use RFC 3339`);
          const text = q.q?.trim().toLowerCase();
          const inProject = (m: Message) => [m.from_name, m.to_name].some((n) => n === `project:${q.project}` || n.startsWith(`${q.project}/`));
          return listMail(
            (m) =>
              (!q.to || m.to === q.to || m.to_name === q.to) &&
              (!q.from || m.from === q.from || m.from_name === q.from) &&
              (!q.project || inProject(m)) &&
              (!q.since || Date.parse(m.created_at) >= Date.parse(q.since)) &&
              (!q.until || Date.parse(m.created_at) <= Date.parse(q.until)) &&
              (!text || [m.subject, m.body, m.from_name, m.to_name].some((x) => x.toLowerCase().includes(text))),
            q,
          );
        }),
      get: (id) => reply(() => render(find(w.mail, (m) => m.id === id, `message ${id}`))),
      send: (m) =>
        reply(() => {
          if (!m.body.trim()) fail(422, "invalid", "the body is empty");
          if (m.from === "system") fail(403, "forbidden", "you cannot send as `system`");
          if (m.thread_id && !w.mail.some((x) => x.thread_id === m.thread_id)) notFound(`thread ${m.thread_id}`);
          return sendMail(m.to, m.from ?? "operator", m.subject ?? "", m.body, m.thread_id);
        }),
      markRead: (id) => reply(() => markRead(id)),
      reply: (id, body, subject) => reply(() => replyTo(id, body, subject)),
      ...inboxMock.mail,
    },

    inbox: inboxMock.inbox,
    threads: threadMock.threads,

    projectInbox: {
      list: (proj, q = {}) => reply(() => (project(proj), listMail((m) => m.to === `project:${proj}`, q))),
      send: (proj, m) => api.mail.send({ ...m, to: `project:${proj}` }),
      markRead: (proj, id) =>
        reply(() => {
          const m = find(w.mail, (x) => x.id === id && x.to === `project:${proj}`, `message ${id}`);
          return markRead(m.id);
        }),
      reply: (proj, id, body, subject) =>
        reply(() => {
          find(w.mail, (x) => x.id === id && x.to === `project:${proj}`, `message ${id}`);
          return replyTo(id, body, subject, `project:${proj}`);
        }),
    },

    audit: (q = {}) =>
      reply(() => {
        const rows = w.audit.filter((a) => (!q.actor || a.actor === q.actor) && (!q.action || a.action === q.action) && (!q.project || a.project === q.project) && (!q.since || a.at >= q.since));
        // `order=desc`: newest first, the cursor goes on to older rows.
        return paginate((q.order === "desc" ? [...rows].reverse() : rows).map(auditLink), q, (a) => String(a.seq ?? a.id));
      }),

    // The daemon's `api/targets.rs` is new; the mock runs the same lifecycle.
    targets: {
      // The mock caller is the operator: `access` is `operator`, and the operator holds no lease.
      list: () => reply(() => w.targets.map(targetView)),
      // docs/api.md §17.2: the operator defines every target. `denyGlobalTargets` acts as a project_admin.
      create: (body) =>
        reply(() => {
          const def = targetDef(body);
          if (!def.project && opts.denyGlobalTargets) fail(403, "forbidden", "defining a global target needs the sysadmin role");
          if (def.project) {
            project(def.project);
            // docs/api.md §17.2: a project target is `<project>-<name>`; a project holds at most TERRARIUM_MAX_TARGETS_PER_PROJECT.
            if (!def.name.startsWith(`${def.project}-`)) def.name = `${def.project}-${def.name}`;
            const max = opts.maxTargetsPerProject ?? 8;
            const n = w.targets.filter((t) => t.project === def.project).length;
            if (n >= max) fail(409, "conflict", `project \`${def.project}\` has ${n} targets, the most it may have (TERRARIUM_MAX_TARGETS_PER_PROJECT = ${max}); delete one first`, { project: def.project, targets: n, max });
          }
          if (w.targets.some((t) => t.name === def.name)) fail(409, "conflict", `target \`${def.name}\` exists`);
          secrets.checkCredential(def);
          const t: Target = { ...def, id: def.name, scope: def.project ? "project" : "global", ephemeral: !!def.ephemeral, keep: !!def.keep, snapshot_before_task: !!def.snapshot_before_task, source: "api", state: "unknown", state_at: null, snapshots: [], usable: true, runtime: {} };
          w.targets.push(t);
          audit("target.create", t.name, t.project, { ...def });
          return targetView(t);
        }),
      // docs/api.md §17.3: each verb runs to its end and answers the executor report.
      action: (id, verb, body) =>
        reply((): TargetReport => {
          const t = target(id);
          const online = !!t.host && w.hosts.find((x) => x.name === t.host)?.status === "online";
          if (t.lab_from_workspace) return instanceVerb(t, verb, body?.agent, online, body);
          const report = (snapshot?: string, scope?: SnapshotScope): TargetReport => {
            t.state_at = iso();
            const r: TargetReport = { target: t.name, verb, state: t.state, ...(snapshot ? { snapshot } : {}), ...(scope ? { scope } : {}) };
            audit(`target.${verb}`, t.name, t.project, { ...r });
            return r;
          };
          if (t.kind === "ssh" || t.kind === "host") {
            if (verb === "snapshot" || verb === "restore") fail(422, "invalid", `a \`${t.kind}\` target has no \`snapshot\``);
            // up and down are no-ops; the state follows the host agent's link.
            t.state = t.kind === "host" && online ? "running" : "unknown";
            return report();
          }
          if (!online) fail(409, "conflict", `host agent \`${t.host ?? "?"}\` of target \`${t.name}\` is offline`);
          if (verb === "status") return report();
          if (verb === "snapshot" || verb === "restore") {
            if (t.kind !== "vmlab") fail(422, "invalid", `a \`${t.kind}\` target has no \`snapshot\``);
            const s = snapVerb(t, t.name, verb, body);
            return report(s.name, s.scope);
          }
          if (verb === "up") {
            if (t.state !== "running" && !t.container) checkRunningCap(t);
            if (t.lab_repo) {
              // docs/api.md §17.8: a worktree of the project's repo at `lab_ref`.
              if (!w.repos.some((r) => r.project === t.project && r.name === t.lab_repo)) fail(422, "invalid", `\`lab_repo\` \`${t.lab_repo}\` is not a repo of project \`${t.project}\``);
              t.runtime = { ...t.runtime, lab: `/var/lib/terrarium-host/instances/${t.name}/lab`, head: "3f2c1e0b9a8d7c6b5a4f3e2d1c0b9a8f7e6d5c4b" };
            }
            t.state = "running";
          } else {
            if (t.ephemeral && !t.keep) {
              // An ephemeral target without keep is destroyed; a destroy clears the snapshots.
              t.state = "absent";
              t.snapshots = [];
              delete w.hostSnapshots[t.name];
            } else t.state = "stopped";
            t.runtime = {};
          }
          return report();
        }),
      exec: (id, req) =>
        reply(() => {
          const t = target(id);
          if (!req.shell && !req.cmd?.length) fail(422, "invalid", "give cmd, or shell: true");
          if (t.kind !== "ssh" && w.hosts.find((h) => h.name === t.host)?.status !== "online") fail(409, "host_offline", `host agent \`${t.host ?? "?"}\` is not connected`);
          const session_id = ulid("exec");
          execSessions.set(session_id, t.name);
          return { session_id, target: t.name, kind: req.shell ? "shell" : "exec" };
        }),
      sessionPty: (id, session) => {
        const path = `/api/targets/${encodeURIComponent(id)}/sessions/${encodeURIComponent(session)}/ws`;
        if (execSessions.get(session) !== id) return sock(path, () => new FakePty({ refuse: true }));
        const t = w.targets.find((x) => x.id === id || x.name === id);
        const user = t?.kind === "ssh" ? (t.address?.split("@")[0] ?? "build") : "wil";
        return sock(path, (protocol) => new FakePty({
          protocol,
          banner: [`\x1b[2m(mock) shell on target ${id} via ${t?.host ?? t?.address ?? "?"}\x1b[0m`],
          prompt: `\x1b[32m${user}@${id}\x1b[0m:~$ `,
          onLine: (line) => (line.trim() === "uname -a" ? [`Linux ${id} 6.12.0 #1 SMP x86_64 GNU/Linux`] : line.trim() ? [`${line.split(" ")[0]}: (mock) ran`] : []),
        }));
      },
      closeSession: (id, session) =>
        reply(() => {
          if (execSessions.get(session) !== id) fail(404, "not_found", `session \`${session}\` not found`);
          execSessions.delete(session);
        }),
      // docs/api.md §17.3: what the host has, merged by name over the machines; the stored names the host lost go.
      snapshots: (id, q = {}) =>
        reply(() => {
          const { t, key, holder } = snapTarget(id, q);
          const scope = snapScope(t, q.scope);
          const machines = scopeMachines(t, scope);
          const snapshots: TargetSnapshot[] = [];
          for (const r of w.hostSnapshots[key] ?? []) {
            if (!machines.includes(r.machine)) continue;
            const x = snapshots.find((s) => s.name === r.name);
            if (x) x.machines.push(r.machine);
            else snapshots.push({ name: r.name, kind: r.kind, taken: r.taken, machines: [r.machine], managed: t.snapshots.includes(r.name) });
          }
          const dropped = t.lab_from_workspace ? [] : t.snapshots.filter((n) => !snapshots.some((s) => s.name === n));
          if (dropped.length) {
            t.snapshots = t.snapshots.filter((n) => !dropped.includes(n));
            audit("target.reconcile", t.name, t.project, { snapshots_dropped: dropped }, "system");
          }
          return { target: t.name, scope, machines, snapshots, dropped, ...(scope === "lab" ? { lab_running: true } : {}), ...(holder ? { agent: holder } : {}) };
        }),
      deleteSnapshot: (id, snap, q = {}) =>
        reply(() => {
          checkSnapName(snap);
          const t0 = target(id);
          if (snap === CLEAN && t0.snapshot_before_task && !q.force) fail(409, "conflict", `target \`${t0.name}\` has \`snapshot_before_task\`: the daemon restores \`${CLEAN}\` when an agent stops. Turn the flag off first, or delete with force=true`);
          const { t, key, holder } = snapTarget(id, q);
          const scope = snapScope(t, q.scope);
          const rows = w.hostSnapshots[key] ?? [];
          let machines = scopeMachines(t, scope);
          if (scope === "lab") {
            machines = machines.filter((m) => rows.some((r) => r.name === snap && r.machine === m));
            if (!machines.length) fail(404, "not_found", `no machine of the lab of target \`${t.name}\` has the snapshot \`${snap}\``);
          }
          for (const m of machines)
            if (!rows.some((r) => r.name === snap && r.machine === m))
              fail(422, "invalid", "vmlab.snapshot_delete failed: exit 1", { verb: "vmlab.snapshot_delete", code: "failed", end: { type: "exit", code: 1, reason: "exited" }, stderr: `"${m}" has no snapshot "${snap}"`, stdout: "" });
          w.hostSnapshots[key] = rows.filter((r) => !(r.name === snap && machines.includes(r.machine)));
          t.snapshots = t.snapshots.filter((n) => n !== snap);
          const out = { target: t.name, verb: "snapshot_delete" as const, snapshot: snap, scope, machines, ...(holder ? { agent: holder } : {}) };
          audit("target.snapshot_delete", t.name, t.project, { ...out });
          return out;
        }),
      leases: (name, o = {}) =>
        reply(() => {
          const t = target(name);
          return leasesWhere((l) => l.target === t.name, o.all);
        }),
      grant: (name, body) =>
        reply(() => {
          const t = target(name);
          if (typeof body?.agent !== "string" || !body.agent.trim()) fail(422, "invalid", "a lease needs `agent`");
          const a = agent(body.agent);
          if (t.project && t.project !== a.project) fail(422, "invalid", `target \`${t.name}\` belongs to project \`${t.project}\`; lease it to an agent of \`${t.project}\``);
          if (body.exclusive && t.lab_from_workspace) fail(422, "invalid", `\`${t.name}\` gives each holder its own lab instance, so an exclusive lease does not apply; grant it without \`exclusive\``);
          const expires_at = parseUntil(body.until);
          sweepLeases();
          const active = w.leases.filter((l) => l.target === t.name && l.state === "active");
          const mine = active.find((l) => l.agent === a.id);
          if (mine) fail(409, "conflict", `${mine.agent_name ?? mine.agent} holds a lease on \`${t.name}\` ${until(mine)} already; PATCH it to extend it, or revoke it first`);
          const others = active.filter((l) => l.agent !== a.id);
          const held = (x: Lease, message: string): never =>
            fail(409, "lease_held", message, { target: t.name, holder: x.agent_name ?? x.agent, holder_id: x.agent, lease_id: x.id, exclusive: x.exclusive, expires_at: x.expires_at });
          const ex = others.find((l) => l.exclusive);
          if (ex) held(ex, `target \`${t.name}\` is held exclusively by ${ex.agent_name ?? ex.agent} ${until(ex)}`);
          if (body.exclusive && others[0]) held(others[0], `target \`${t.name}\` is leased to ${others[0].agent_name ?? others[0].agent} ${until(others[0])}; an exclusive lease needs no other holder`);
          const l: Lease = {
            id: ulid("lease"),
            target: t.name,
            agent: a.id,
            agent_name: `${a.project}/${a.name}`,
            project: a.project,
            granted_by: "operator",
            granted_at: iso(),
            expires_at,
            exclusive: !!body.exclusive,
            revoked_at: null,
            revoked_by: null,
            reason: body.reason?.trim() || null,
            state: "active",
          };
          w.leases.push(l);
          audit("target.grant", t.name, l.project, { lease: l.id, agent: l.agent, agent_name: l.agent_name, expires_at, exclusive: l.exclusive, reason: l.reason });
          leaseEvent(l, "granted", "operator");
          armExpiry(l);
          // A grant to a running agent brings an ephemeral target up (`ensure_up`).
          const holders = w.leases.filter((x) => x.target === t.name && x.state === "active" && x.agent !== a.id).some((x) => LIVE.includes(w.agents.find((y) => y.id === x.agent)?.status ?? "stopped"));
          if (t.ephemeral && LIVE.includes(a.status) && !holders && t.state !== "running") {
            if (w.hosts.find((h) => h.name === t.host)?.status !== "online") return { ...l, lifecycle_error: `host agent \`${t.host ?? "?"}\` is not connected` };
            t.state = "running";
            t.state_at = iso();
            return { ...l, lifecycle: { target: t.name, verb: "up", state: "running" } };
          }
          return l;
        }),
      extendLease: (name, key, untilRaw) =>
        reply(() => {
          const l = activeLease(target(name), agent(key));
          l.expires_at = parseUntil(untilRaw);
          audit("target.lease_extend", l.target, l.project, { lease: l.id, agent: l.agent, expires_at: l.expires_at });
          leaseEvent(l, "extended", "operator");
          armExpiry(l);
          return l;
        }),
      revokeLease: (name, key) =>
        reply(() => {
          const l = activeLease(target(name), agent(key));
          l.state = "revoked";
          l.revoked_at = iso();
          l.revoked_by = "operator";
          audit("target.revoke", l.target, l.project, { lease: l.id, agent: l.agent, agent_name: l.agent_name });
          leaseEvent(l, "ended", "operator");
          return l;
        }),
      // docs/api.md §17.10: the mock caller is the operator, so input is allowed.
      displayInfo: (name) =>
        reply((): DisplayInfo => {
          const t = displayTarget(name);
          const ops: DisplayOpName[] = t.kind === "vmlab"
            ? ["info", "screenshot", "move", "click", "drag", "keys", "type", "ocr", "find", "record"]
            : ["info", "screenshot", "move", "click", "drag", "scroll", "keys", "type"];
          return { target: t.name, kind: t.kind, backend: t.kind, width: MOCK_SCREEN.width, height: MOCK_SCREEN.height, ops, ocr: t.kind === "vmlab" ? "vmlab" : null, input: { allowed: true, reason: null } };
        }),
      screenshot: (name, o = {}) =>
        reply((): DisplayShot => {
          const t = displayTarget(name);
          const scale = o.scale ?? null;
          if (scale !== null && !(scale >= 0.1 && scale <= 1)) fail(422, "invalid", `scale ${scale} is outside 0.1..=1`);
          const r = o.region ?? null;
          const w0 = r ? r.w : MOCK_SCREEN.width;
          const h0 = r ? r.h : MOCK_SCREEN.height;
          const f = scale ?? 1;
          return {
            target: t.name, op: "screenshot", format: "png",
            width: Math.max(1, Math.round(w0 * f)), height: Math.max(1, Math.round(h0 * f)),
            screen: { ...MOCK_SCREEN }, region: r, scale,
            bytes: MOCK_PNG_BYTES, sha256: "0".repeat(64), png_base64: MOCK_PNG,
          };
        }),
      displayOp: (name, op, body = {}) =>
        reply((): DisplayOpResult => {
          const t = displayTarget(name);
          if (!MOCK_OPS.includes(op)) fail(404, "not_found", `no display operation \`${op}\``);
          if (op === "scroll" && t.kind === "vmlab") fail(422, "invalid", "vmlab has no scroll input; use keys pagedown or pageup", { code: "unsupported" });
          if (["move", "click", "drag", "scroll", "keys", "type", "record"].includes(op)) {
            const detail = op === "type" ? { op, chars: String(body.text ?? "").length } : { op, ...body };
            audit("target.display", t.name, t.project, detail);
          }
          return { target: t.name, op, ok: true, ...(op === "type" ? { chars: String(body.text ?? "").length } : body) };
        }),
      // docs/api.md §17.2: a JSON merge patch. A running target takes only `keep` and `snapshot_before_task`.
      update: (name, patch) =>
        reply(() => {
          const t = target(name);
          if (t.source === "file") fail(409, "file_managed", `target \`${t.name}\` is declared in targets/${t.name}.toml; edit the file, then POST /api/reload`);
          const next: Record<string, unknown> = { ...t };
          for (const [k, v] of Object.entries(patch)) {
            if (k === "name" && v !== t.name) fail(422, "invalid", "a target cannot change its name");
            if (v === null) delete next[k];
            else if (k === "limits" && t.limits && typeof v === "object") {
              // A merge patch: the limits that the patch does not name stay; `null` removes one.
              const merged: Record<string, unknown> = { ...t.limits };
              for (const [lk, lv] of Object.entries(v as Record<string, unknown>)) if (lv === null) delete merged[lk];
              else merged[lk] = lv;
              if (Object.keys(merged).length) next[k] = merged;
              else delete next[k];
            } else if (v !== undefined) next[k] = v;
          }
          const changed = Object.keys(patch).filter((k) => JSON.stringify(next[k] ?? null) !== JSON.stringify((t as unknown as Record<string, unknown>)[k] ?? null));
          const running = t.state === "running" || Object.values(t.runtime?.instances ?? {}).some((i) => i.state === "running");
          // A host target is `running` while its host agent is online; its limits apply to each new session, so they change.
          const locked = changed.filter((k) => k !== "keep" && k !== "snapshot_before_task" && !(k === "limits" && t.kind === "host"));
          if (running && locked.length) fail(409, "conflict", `target \`${t.name}\` runs: bring it down first to change ${locked.join(", ")}`, { state: "running", fields: locked });
          const def = targetDef({ ...(next as unknown as NewTarget), name: t.name });
          secrets.checkCredential(def, t);
          for (const k of Object.keys(patch)) {
            if (next[k] === undefined) delete (t as unknown as Record<string, unknown>)[k];
            else (t as unknown as Record<string, unknown>)[k] = next[k];
          }
          if (changed.length) audit("target.update", t.name, t.project, { fields: changed });
          return targetView(t);
        }),
      test: (name) => reply(() => secrets.test(targetView(target(name)))),
    },

    waits: waitMock.waits,
    questions: questionMock.questions,
    watches: waitMock.watches,

    schedules: createScheduleMock({
      w,
      now,
      iso,
      ulid,
      reply,
      fail,
      emit,
      audit,
      agent,
      target,
      sub,
      project,
      sendMail: (to, from, subject, body) => sendMail(to, from, subject, body),
      runMs: opts.scheduleRunMs ?? (latency > 0 ? 1500 : step),
      caller: opts.scheduleCaller,
      webhookSecret: (project, name) => secrets.webhookSecret(project, name),
    }),

    // The same caller as the schedules: the rights of §20 follow those of §19.
    eventSources: createEventSourceMock({ w, iso, ulid, reply, fail, emit, audit, caller: opts.scheduleCaller }),

    // PLAN §16b, docs/api.md §5.2.
    secrets: secrets.api,

    // The rights of §21 follow those of §20, but every caller reads the global templates.
    templates: createTemplateMock({ w, iso, ulid, reply, fail, emit, audit, agent, caller: opts.scheduleCaller }),

    // PLAN §8.3, docs/api.md §25: a small in-memory config repo.
    config: createConfigMock({ iso, reply, fail, audit, available: opts.configRepo, caller: opts.scheduleCaller }),

    // Terrarium-managed skills, in the config repo (./skills).
    tools: toolMock.api,
    skills: skillMock.api,

    // The system prompt library, in the config repo (./systemPrompts).
    systemPrompts: systemPromptMock.api,

    // The project DB (./db).
    db: dbMock.api,
    testRuns: testRunMock.api,
    rules: rulesMock.api,
    monitors: monitorMock.api,
    pipelines: pipelineMock.api,
    calendar: calendarMock.api,

    projectFolders: folderMock.api,

    // docs/api.md §6.11: operator only; each call answers with the list.
    pins: (() => {
      const pinList = (): Pin[] =>
        w.pins.flatMap((p) => {
          const a = w.agents.find((x) => x.id === p.agent);
          return a ? [{ agent: a.id, project: a.project, name: a.name, pinned_at: p.pinned_at }] : [];
        });
      const operatorOnly = () => {
        const c = opts.scheduleCaller;
        if (c && c.role !== "operator") fail(403, "forbidden", "pinned conversations needs the operator");
      };
      return {
        list: () =>
          reply(() => {
            operatorOnly();
            return pinList();
          }),
        pin: (key: string) =>
          reply(() => {
            operatorOnly();
            const a = agent(key);
            if (!w.pins.some((p) => p.agent === a.id)) {
              w.pins.push({ agent: a.id, pinned_at: iso() });
              audit("agent.pin", a.id, a.project, { owner: "operator" });
            }
            return pinList();
          }),
        unpin: (key: string) =>
          reply(() => {
            operatorOnly();
            const id = w.agents.find((x) => x.id === key || `${x.project}/${x.name}` === key)?.id ?? key;
            if (w.pins.some((p) => p.agent === id)) {
              w.pins = w.pins.filter((p) => p.agent !== id);
              audit("agent.unpin", id, w.agents.find((x) => x.id === id)?.project, { owner: "operator" });
            }
            return pinList();
          }),
      };
    })(),

    hosts: {
      list: () => reply(() => w.hosts.map((h) => withMetrics(h))),
      create: (name, opts) =>
        reply(() => {
          if (!NAME_RE.test(name)) fail(422, "invalid", `\`${name}\` is not a valid host name`);
          if (w.hosts.some((h) => h.name === name)) fail(409, "conflict", `host \`${name}\` exists`);
          const enrol = !!opts?.enrol;
          const token = `${enrol ? "the" : "thk"}_${ulid("tok").toLowerCase()}`;
          const id = ulid("host");
          const enrolUntil = new Date(Date.now() + 10 * 60_000).toISOString();
          // As the daemon's `hosts::offline_reason` for a host that never dialled in.
          const offline_reason = opts?.enrol
            ? `waiting for the host agent to dial in with its enrol token (it runs out at ${enrolUntil})`
            : "never connected: the host agent has not reached the daemon. Check `daemon_url` in /etc/terrarium-host/config.toml and `journalctl -u terrarium-host` on the host";
          const host: Host = { name, id, host_id: id, status: "offline", online: false, offline_reason, sessions: 0, capabilities: {}, summary: { os: "", arch: "", docker: false, vmlab: false }, last_seen: null, os: null, arch: null, version: null, stale: false, created_at: iso(), created_by: "operator" };
          w.hosts.push(host);
          w.hostTokens[name] = token;
          audit("host.create", name, undefined, { host_id: id, token_kind: enrol ? "enrol" : "host" });
          if (!enrol) return { ...host, token, token_kind: "host" as const };
          // As the daemon's api/hosts.rs: the install line from its public URL.
          return {
            ...host,
            token,
            token_kind: "enrol" as const,
            token_expires_at: enrolUntil,
            install_command: installCommand(MOCK_PUBLIC_URL, token, name),
          };
        }),
      revoke: (name) =>
        reply(() => {
          const h = find(w.hosts, (x) => x.name === name, `host \`${name}\``);
          w.hosts.splice(w.hosts.indexOf(h), 1);
          delete w.hostTokens[name];
          audit("host.revoke", name, undefined);
        }),
      // As the daemon's api/hosts.rs: 1 to 60 minutes, oldest first; none for an older host agent.
      metrics: (name, opts = {}) =>
        reply(() => {
          const minutes = opts.minutes ?? 60;
          if (!Number.isInteger(minutes) || minutes < 1 || minutes > 60) fail(400, "bad_request", "minutes must be 1 to 60");
          const h = find(w.hosts, (x) => x.name === name, `host \`${name}\``);
          const samples = h.capabilities?.metrics && h.status === "online" ? mockHistory(name, minutes) : [];
          return { host: name, interval_secs: 10, minutes, stale: samples.length === 0, samples };
        }),
    },

    events: {
      stream: (o = {}): EventStream => {
        ensureActivity();
        return createWsEventStream((since) => `/api/events?audit=1${since === undefined ? "" : `&since=${since}`}`, {
          since: o.since,
          makeSocket: (path) => sock(path, (protocol, h) => new MockEventSocket(h.url, protocol)),
        });
      },
      history: (q = {}) =>
        reply(() => eventsAfter(q, (e) => !q.agent || e.agent_id === q.agent || e.agent === q.agent || `${e.project}/${e.agent}` === q.agent)),
      latest: () => reply(() => w.seq),
    },
  };
  const sim: MockSim = {
    setWindow,
    threadPost: (id, text) => threadMock.agentPost(id, text),
    threadAsk: (id, q) => threadMock.agentAsk(id, q),
    threadPropose: (id, body) => threadMock.agentPropose(id, body),
    offerLogins: () => {
      for (const d of detected()) {
        const { suggested, ...login } = d;
        emit({ kind: "subscription.offer", actor: "system", summary: `Found a ${d.harness} login for ${d.provider} with no subscription`, data: { login, suggested } });
      }
    },
  };
  guard(api, authorize);
  return Object.assign(api, { sim });
}

/**
 * Put the auth check in front of every HTTP method of the mock. The verb
 * comes from the real client (./verbs), so the mock and the transport agree
 * on what mutates. Sockets check at the upgrade (./ws). `session.*` checks
 * itself.
 */
function guard(api: object, authorize: (verb: string) => void): void {
  const verbs = httpVerbs();
  const namespaces = new Set([...verbs.keys()].filter((k) => k.includes(".")).map((k) => k.split(".")[0]!));
  const wrap = (obj: Record<string, unknown>, prefix: string) => {
    for (const [key, value] of Object.entries(obj)) {
      const name = `${prefix}${key}`;
      if (typeof value === "function") {
        const verb = verbs.get(name);
        if (!verb || verb === "WS" || verb === "SSE" || name.startsWith("session.")) continue;
        const fn = value as (...a: unknown[]) => unknown;
        obj[key] = (...args: unknown[]) => {
          try {
            authorize(verb);
          } catch (e) {
            return Promise.reject(e);
          }
          return fn(...args);
        };
      } else if (!prefix && namespaces.has(key) && typeof value === "object" && value !== null) {
        wrap(value as Record<string, unknown>, `${name}.`);
      }
    }
  };
  wrap(api as Record<string, unknown>, "");
}
