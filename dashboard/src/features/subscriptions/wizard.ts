// The state machine of the Add subscription wizard (PLAN §16.1, docs/api.md
// §16.4). Pure: the component sends actions and draws the state.
//
//   pick ──pick──▶ setup ──start──▶ starting ──started──▶ live ──polled(done)──▶ done
//                    ▲                 │                    │
//                    └──── failed ─────┘                    └─polled(failed|cancelled|expired)─▶ ended ──retry──▶ setup
//
// `api_key` and `define` are forms: setup ──saved──▶ done.

import type { ApiKeyInput, LoginKind, LoginSession, Subscription } from "../../api/types";

export type WizardPath = LoginKind | "api_key" | "define";

export interface WizardSetup {
  /** The subscription to make or update. Absent: the daemon picks a name from the status output. */
  name?: string;
  /** For `pi`: the pi provider. For `api_key`: the key provider. */
  provider?: string;
}

export type WizardState =
  | { step: "pick" }
  | { step: "setup"; path: WizardPath; setup: WizardSetup }
  | { step: "starting"; path: LoginKind; setup: WizardSetup }
  | { step: "live"; path: LoginKind; setup: WizardSetup; session: LoginSession }
  | { step: "ended"; path: LoginKind; setup: WizardSetup; session: LoginSession; message: string }
  | { step: "done"; subscription: string; session?: LoginSession };

export type WizardAction =
  | { type: "pick"; path: WizardPath; setup?: WizardSetup }
  | { type: "back" }
  | { type: "setup"; setup: WizardSetup }
  | { type: "start" }
  | { type: "failed" }
  | { type: "started"; session: LoginSession }
  | { type: "polled"; session: LoginSession }
  | { type: "saved"; subscription: string }
  | { type: "retry" };

export const isLoginPath = (p: WizardPath): p is LoginKind => p !== "api_key" && p !== "define";

export function initialWizard(path?: WizardPath, setup: WizardSetup = {}): WizardState {
  return path ? { step: "setup", path, setup } : { step: "pick" };
}

/** Why a finished session did not make a subscription. */
export function endMessage(s: LoginSession): string {
  switch (s.state) {
    case "failed":
      return `The login failed${s.error ? `: ${s.error}` : "."}`;
    case "cancelled":
      return "The login was cancelled.";
    case "expired":
      return "The login terminal closed after ten minutes, or the daemon restarted. Start again.";
    default:
      return "The login ended.";
  }
}

/** A session that the daemon reports. `done` true ends the flow. */
function fromSession(state: { path: LoginKind; setup: WizardSetup }, session: LoginSession): WizardState {
  if (session.done) {
    if (session.state === "done" && session.subscription) return { step: "done", subscription: session.subscription, session };
    return { step: "ended", path: state.path, setup: state.setup, session, message: endMessage(session) };
  }
  return { step: "live", path: state.path, setup: state.setup, session };
}

export function wizardReduce(s: WizardState, a: WizardAction): WizardState {
  switch (a.type) {
    case "pick":
      return s.step === "pick" ? { step: "setup", path: a.path, setup: a.setup ?? {} } : s;
    case "back":
      return s.step === "setup" || s.step === "ended" ? { step: "pick" } : s;
    case "setup":
      return s.step === "setup" ? { ...s, setup: { ...s.setup, ...a.setup } } : s;
    case "start":
      return s.step === "setup" && isLoginPath(s.path) ? { step: "starting", path: s.path, setup: s.setup } : s;
    case "failed":
      return s.step === "starting" ? { step: "setup", path: s.path, setup: s.setup } : s;
    case "started":
      return s.step === "starting" ? fromSession(s, a.session) : s;
    case "polled":
      // A late answer for another session, or after the flow ended, is dropped.
      return s.step === "live" && s.session.id === a.session.id ? fromSession(s, a.session) : s;
    case "saved":
      return s.step === "setup" && !isLoginPath(s.path) ? { step: "done", subscription: a.subscription } : s;
    case "retry":
      return s.step === "ended" ? { step: "setup", path: s.path, setup: s.setup } : s;
  }
}

/** Poll `GET .../login/{id}` while this is true. */
export const shouldPoll = (s: WizardState): boolean => s.step === "live" && !s.session.done;

/** On close, cancel with `DELETE .../login/{id}` while this is set. */
export const sessionToCancel = (s: WizardState): string | null => (s.step === "live" && !s.session.done ? s.session.id : null);

export const TEARDOWN_MS = 10 * 60_000;

/** Time left before the daemon tears the login terminal down. */
export function teardownLeft(s: Pick<LoginSession, "started_at">, now = Date.now()): number {
  const start = Date.parse(s.started_at);
  if (Number.isNaN(start)) return TEARDOWN_MS;
  return Math.max(0, start + TEARDOWN_MS - now);
}

/** `m:ss`, for the countdown. */
export function countdown(ms: number): string {
  const s = Math.max(0, Math.ceil(ms / 1000));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

/** Device-code flows show the code large. Paste flows (`claude`, `pi`) take a code from the browser. */
export const isDeviceFlow = (k: LoginKind): boolean => k === "codex" || k === "copilot";
export const takesCode = (k: LoginKind): boolean => k === "claude" || k === "pi";

/** The pi providers that `GET /api/subscriptions/detect` checks. */
export const PI_PROVIDERS = ["anthropic", "openai-codex", "github-copilot", "google-gemini-cli"] as const;

/** The providers of `POST /api/subscriptions/{name}/api-key` and their env vars (docs/api.md §16.5). */
export const KEY_PROVIDERS: { id: string; env: string; label: string }[] = [
  { id: "anthropic", env: "ANTHROPIC_API_KEY", label: "Anthropic" },
  { id: "openai", env: "OPENAI_API_KEY", label: "OpenAI" },
  { id: "openai-compatible", env: "OPENAI_API_KEY + OPENAI_BASE_URL", label: "OpenAI-compatible endpoint" },
  { id: "google", env: "GEMINI_API_KEY", label: "Google" },
  { id: "openrouter", env: "OPENROUTER_API_KEY", label: "OpenRouter" },
  { id: "xai", env: "XAI_API_KEY", label: "xAI" },
  { id: "groq", env: "GROQ_API_KEY", label: "Groq" },
  { id: "github", env: "COPILOT_GITHUB_TOKEN", label: "GitHub (fine-grained PAT with Copilot Requests)" },
  { id: "zai", env: "ZAI_API_KEY (pi) and ANTHROPIC_AUTH_TOKEN (Claude Code)", label: "Z.ai (GLM)" },
];

/** The plan that a Z.ai key gets when the form names none. */
export const ZAI_DEFAULT_PLAN = "coding";

/** The plans of a Z.ai key (docs/api.md §16.5). The first is the default. */
export const ZAI_PLANS: { id: string; label: string }[] = [
  { id: "coding", label: "GLM Coding Plan" },
  { id: "api", label: "Pay as you go (API)" },
];

/** Where to get the key, for the providers that need a word of help. */
export const KEY_HELP: Record<string, string> = {
  zai: "Make a key at z.ai/manage-apikey/apikey-list. The GLM Coding Plan and pay as you go use the same key; the plan picks the endpoint. Agents run on Claude Code and pi.",
};

/** True when the provider takes a plan. */
export const takesPlan = (provider: string): boolean => provider === "zai";

/** The body of `POST /api/subscriptions/{name}/api-key` from the form fields. */
export function apiKeyBody(f: { provider: string; key: string; plan?: string; baseUrl?: string; models?: string }): ApiKeyInput {
  const models = (f.models ?? "").split(",").map((m) => m.trim()).filter(Boolean);
  return {
    provider: f.provider,
    key: f.key,
    ...(takesPlan(f.provider) ? { plan: f.plan || ZAI_DEFAULT_PLAN } : {}),
    ...(f.provider === "openai-compatible" ? { base_url: (f.baseUrl ?? "").trim() } : {}),
    ...(models.length ? { models } : {}),
  };
}

/** The wizard path and setup for **Re-login** on a subscription. */
export function reloginFor(s: Pick<Subscription, "name" | "provider" | "harnesses" | "env_var" | "has_api_key">): { path: WizardPath; setup: WizardSetup } {
  if (s.has_api_key || s.env_var) return { path: "api_key", setup: { name: s.name, provider: s.provider } };
  for (const k of ["claude", "codex", "copilot"] as const) if (s.harnesses.includes(k)) return { path: k, setup: { name: s.name } };
  return { path: "pi", setup: { name: s.name, provider: s.provider } };
}
