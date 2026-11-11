// The state machine of the Add subscription wizard (PLAN §16.1, docs/api.md
// §16.4). Pure: the component sends actions and draws the state.
//
//   pick ──pick──▶ setup ──start──▶ starting ──started──▶ live ──polled(done)──▶ done
//                    ▲                 │                    │
//                    └──── failed ─────┘                    └─polled(failed|cancelled|expired)─▶ ended ──retry──▶ setup
//
// `api_key` and `define` are forms: setup ──saved──▶ done.

import type { ApiKeyInput, Harness, LoginKind, LoginSession, NewSubscription, SelfHosted, SelfHostedApi, Subscription } from "../../api/types";

/** The logins of the wizard. pi uses API keys only, so it has none (its API key is the `api_key` path). */
export type WizardLogin = Exclude<LoginKind, "pi">;

export type WizardPath = WizardLogin | "api_key" | "define" | "selfhosted";

export interface WizardSetup {
  /** The subscription to make or update. Absent: the daemon picks a name from the status output. */
  name?: string;
  /** For `api_key`: the key provider. */
  provider?: string;
}

export type WizardState =
  | { step: "pick" }
  | { step: "setup"; path: WizardPath; setup: WizardSetup }
  | { step: "starting"; path: WizardLogin; setup: WizardSetup }
  | { step: "live"; path: WizardLogin; setup: WizardSetup; session: LoginSession }
  | { step: "ended"; path: WizardLogin; setup: WizardSetup; session: LoginSession; message: string }
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

export const isLoginPath = (p: WizardPath): p is WizardLogin => p !== "api_key" && p !== "define" && p !== "selfhosted";

/** The wizard that `?add=<path>` opens (the agent form links `?add=api_key`), or `null`. */
export function wizardFromQuery(add: string | string[] | undefined): { path: WizardPath } | null {
  const p = Array.isArray(add) ? add[0] : add;
  const paths: WizardPath[] = ["api_key", "define", "selfhosted", "claude", "codex", "copilot"];
  return p && (paths as string[]).includes(p) ? { path: p as WizardPath } : null;
}

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
function fromSession(state: { path: WizardLogin; setup: WizardSetup }, session: LoginSession): WizardState {
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

/** Device-code flows show the code large. The paste flow (`claude`) takes a code from the browser. */
export const isDeviceFlow = (k: WizardLogin): boolean => k === "codex" || k === "copilot";
export const takesCode = (k: WizardLogin): boolean => k === "claude";

/**
 * The providers of `POST /api/subscriptions/{name}/api-key` and their env vars (docs/api.md §16.5).
 * `harnesses`: the harnesses the daemon gives the new subscription; the form shows them.
 */
export const KEY_PROVIDERS: { id: string; env: string; label: string; harnesses?: string[] }[] = [
  { id: "litellm", env: "ANTHROPIC_AUTH_TOKEN", label: "LiteLLM (Claude Code)", harnesses: ["claude"] },
  { id: "gcp", env: "GOOGLE_APPLICATION_CREDENTIALS (0600 JSON file)", label: "Google Cloud Vertex AI (Claude Code)", harnesses: ["claude"] },
  { id: "foundry", env: "ANTHROPIC_FOUNDRY_API_KEY", label: "Microsoft Foundry (Claude Code)", harnesses: ["claude"] },
  { id: "bedrock", env: "AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY", label: "Amazon Bedrock (Claude Code)", harnesses: ["claude"] },
  { id: "anthropic", env: "ANTHROPIC_API_KEY", label: "Anthropic" },
  { id: "openai", env: "OPENAI_API_KEY", label: "OpenAI" },
  { id: "google", env: "GEMINI_API_KEY", label: "Google" },
  { id: "openrouter", env: "OPENROUTER_API_KEY", label: "OpenRouter" },
  { id: "xai", env: "XAI_API_KEY", label: "xAI" },
  { id: "groq", env: "GROQ_API_KEY", label: "Groq" },
  { id: "selfhosted", env: "TERRARIUM_SELFHOSTED_API_KEY (and ANTHROPIC_AUTH_TOKEN for an Anthropic-style server)", label: "Self-hosted model server (its optional key)" },
  { id: "github", env: "COPILOT_GITHUB_TOKEN", label: "GitHub (fine-grained PAT with Copilot Requests)" },
  { id: "zai", env: "ZAI_API_KEY (pi) and ANTHROPIC_AUTH_TOKEN (Claude Code)", label: "Z.ai (GLM)", harnesses: ["claude", "pi"] },
  { id: "moonshot", env: "MOONSHOT_API_KEY (and KIMI_API_KEY for Kimi Code)", label: "Moonshot AI / Kimi (pi only)", harnesses: ["pi"] },
];

/** The plan that a Z.ai key gets when the form names none. */
export const ZAI_DEFAULT_PLAN = "coding";

/** The plans of a Z.ai key (docs/api.md §16.5). The first is the default. */
export const ZAI_PLANS: { id: string; label: string }[] = [
  { id: "coding", label: "GLM Coding Plan" },
  { id: "api", label: "Pay as you go (API)" },
];

/** The plan that a Moonshot key gets when the form names none. */
export const MOONSHOT_DEFAULT_PLAN = "coding";

/** The plans of a Moonshot key (docs/api.md §16.5): the product and the region. The first is the default. */
export const MOONSHOT_PLANS: { id: string; label: string }[] = [
  { id: "coding", label: "Kimi Code membership (api.kimi.ai)" },
  { id: "coding-cn", label: "Kimi Code membership, China (api.kimi.com)" },
  { id: "api", label: "Open Platform, pay as you go (api.moonshot.ai)" },
  { id: "api-cn", label: "Open Platform, pay as you go, China (api.moonshot.cn)" },
];

/** The plans of each provider that takes one. */
export const KEY_PLANS: Record<string, { id: string; label: string }[]> = { zai: ZAI_PLANS, moonshot: MOONSHOT_PLANS };

/** The default plan of a provider that takes one. */
export const defaultPlan = (provider: string): string => KEY_PLANS[provider]?.[0]?.id ?? "";

/** Where to get the key, for the providers that need a word of help. */
export const KEY_HELP: Record<string, string> = {
  gcp: "Enable the Vertex AI API and Claude models in Model Garden. Use a service-account JSON key with the Vertex AI User role (roles/aiplatform.user) on the request project. The sandbox receives a 0600 file, not JSON in an env var. Saving tests Haiku with a paid, one-output-token call before storing anything. Defaults: Opus 5.5, Sonnet 4.6, Haiku 4.5. Model access varies by region.",
  foundry: "Deploy Claude models in Microsoft Foundry and use the resource API key. Defaults: claude-opus-5-5, claude-sonnet-4-6, claude-haiku-4-5. Override these if your deployment names differ. Saving a resource tests Haiku with a paid, one-output-token call before storing anything. A custom base URL is saved without a test call.",
  bedrock: "Use an IAM access key pair with bedrock:InvokeModel and bedrock:InvokeModelWithResponseStream on your permitted models and inference profiles. Saving tests the haiku profile with a paid, one-output-token call to the regional Bedrock endpoint before storing the key. US inference profiles are the defaults. For other geographies, enter profile IDs or ARNs for all three models. Gateway subscriptions with cloud.base_url cannot be tested here: save via the API without test_connection instead.",
  selfhosted: "The key of a self-hosted server that needs one. Add a new server with the Self-hosted model path: it sets the API style, the models, and the limits.",
  zai: "Make a key at z.ai/manage-apikey/apikey-list. The GLM Coding Plan and pay as you go use the same key; the plan picks the endpoint. Agents run on Claude Code and pi.",
  moonshot:
    "For pi agents only. A Kimi Code key comes from kimi.com/code/console; an Open Platform key comes from platform.kimi.ai (China: platform.kimi.com). The keys are different, and a key works only in its own region. The plan picks the endpoint.",
};

/** True when the provider takes a plan. */
export const takesPlan = (provider: string): boolean => provider in KEY_PLANS;

/** The body of `POST /api/subscriptions/{name}/api-key` from the form fields. */
export function apiKeyBody(f: { provider: string; key: string; plan?: string; baseUrl?: string; models?: string; keyId?: string; region?: string; projectId?: string; resource?: string; cloudModels?: Record<string, string> }): ApiKeyInput {
  if (f.provider === "foundry") return {
    provider: "foundry", key: f.key,
    cloud: { ...(f.baseUrl?.trim() ? { base_url: f.baseUrl.trim() } : { resource: f.resource?.trim() }), models: Object.fromEntries(Object.entries(f.cloudModels ?? {}).filter(([, v]) => v.trim()).map(([k, v]) => [k, v.trim()])) },
    test_connection: !f.baseUrl?.trim(),
  };
  if (f.provider === "gcp") return {
    provider: "gcp", key: f.key,
    cloud: { region: f.region?.trim() || "us-east5", project_id: f.projectId?.trim(), models: Object.fromEntries(Object.entries(f.cloudModels ?? {}).filter(([, v]) => v.trim()).map(([k, v]) => [k, v.trim()])) },
    test_connection: true,
  };
  if (f.provider === "bedrock") return {
    provider: "bedrock", key: f.key, key_id: f.keyId?.trim(),
    cloud: { region: f.region?.trim() || "us-east-1", models: Object.fromEntries(Object.entries(f.cloudModels ?? {}).filter(([, v]) => v.trim()).map(([k, v]) => [k, v.trim()])) },
    test_connection: true,
  };
  const models = (f.models ?? "").split(",").map((m) => m.trim()).filter(Boolean);
  return {
    provider: f.provider,
    key: f.key,
    ...(takesPlan(f.provider) ? { plan: f.plan && KEY_PLANS[f.provider]?.some((p) => p.id === f.plan) ? f.plan : defaultPlan(f.provider) } : {}),
    ...(f.provider === "litellm" || f.provider === "openai-compatible" || (f.provider === "selfhosted" && f.baseUrl?.trim()) ? { base_url: (f.baseUrl ?? "").trim() } : {}),
    ...(models.length ? { models } : {}),
  };
}

/** The wizard path and setup for **Re-login** on a subscription. pi uses API keys only: a pi subscription stores a key again. */
export function reloginFor(s: Pick<Subscription, "name" | "provider" | "harnesses" | "env_var" | "has_api_key">): { path: WizardPath; setup: WizardSetup } {
  // A self-hosted server: store (or replace) its optional key.
  if (isSelfHosted(s.provider)) return { path: "api_key", setup: { name: s.name, provider: "selfhosted" } };
  if (s.has_api_key || s.env_var) return { path: "api_key", setup: { name: s.name, provider: s.provider } };
  for (const k of ["claude", "codex", "copilot"] as const) if (s.harnesses.includes(k)) return { path: k, setup: { name: s.name } };
  return { path: "api_key", setup: { name: s.name, provider: s.provider } };
}

// ----- Self-hosted model servers (docs/api.md §16) -----

/** True for a `selfhosted` subscription (`openai-compatible` is its old word). */
export const isSelfHosted = (provider: string | undefined): boolean => {
  const p = (provider ?? "").trim().toLowerCase();
  return p === "selfhosted" || p === "openai-compatible";
};

/** The harnesses of each API style. Copilot never. */
export const STYLE_HARNESSES: Record<SelfHostedApi, Harness[]> = { openai: ["pi", "codex"], anthropic: ["claude", "pi"] };

/** The API styles, with the form of the base URL of each. */
export const STYLES: { id: SelfHostedApi; label: string; hint: string; placeholder: string }[] = [
  { id: "openai", label: "OpenAI style", hint: "The OpenAI root, with /v1 (vLLM, Ollama, llama.cpp, SGLang, LM Studio). pi and Codex use it; Codex needs the Responses API (/v1/responses).", placeholder: "http://gpu.lan:8000/v1" },
  { id: "anthropic", label: "Anthropic style", hint: "The Anthropic root, without /v1 (the server answers /v1/messages). Claude Code and pi use it.", placeholder: "http://gpu.lan:8000" },
];

/** The form of the Self-hosted model path. Numbers are the text of their fields. */
export interface SelfHostedForm {
  name: string;
  api: SelfHostedApi;
  baseUrl: string;
  key: string;
  /** Model ids, split on commas and new lines. */
  models: string;
  defaultModel: string;
  contextWindow: string;
  maxOutputTokens: string;
  maxConcurrent: string;
  statusUrl: string;
  harnesses: Harness[];
}

export const emptySelfHostedForm = (): SelfHostedForm => ({
  name: "",
  api: "openai",
  baseUrl: "",
  key: "",
  models: "",
  defaultModel: "",
  contextWindow: "",
  maxOutputTokens: "",
  maxConcurrent: "",
  statusUrl: "",
  harnesses: [...STYLE_HARNESSES.openai],
});

/** The model ids of a text: split on commas and new lines, no empty or repeated id. */
export const parseModels = (text: string): string[] => [...new Set(text.split(/[,\n]/).map((m) => m.trim()).filter(Boolean))];

/** Why `url` is not an `http(s)` URL the daemon takes, or null. */
export function urlError(what: string, url: string, allowQuery = false): string | null {
  const u = url.trim();
  if (!u) return `${what} is empty`;
  if (/\s/.test(u)) return `${what} has white space`;
  const m = /^(https?):\/\/([^/?#]*)(.*)$/i.exec(u);
  if (!m) return `${what} must start with http:// or https://`;
  if (!m[2] || m[2].startsWith(":")) return `${what} has no host`;
  if (m[2].includes("@")) return `${what} must not hold a user or a password`;
  if (!allowQuery && /[?#]/.test(m[3] ?? "")) return `${what} must not have a query or a fragment`;
  return null;
}

/** A whole number from a field: `undefined` for an empty field, else the number or an error text. */
export function parseCount(what: string, text: string, min: number, max = Number.MAX_SAFE_INTEGER): { value?: number } | { error: string } {
  const t = text.trim().replace(/_/g, "");
  if (!t) return {};
  const k = /^(\d+)\s*([kKmM]?)$/.exec(t);
  if (!k) return { error: `${what} is not a whole number` };
  const n = Number(k[1]) * (k[2]?.toLowerCase() === "k" ? 1024 : k[2]?.toLowerCase() === "m" ? 1_048_576 : 1);
  if (n < min || n > max) return { error: `${what} is ${n}; use ${min} to ${max === Number.MAX_SAFE_INTEGER ? "any larger number" : max}` };
  return { value: n };
}

/** The harnesses of the form that the style serves (a change of style drops the others). */
export const styleHarnesses = (api: SelfHostedApi, picked: Harness[]): Harness[] => picked.filter((h) => STYLE_HARNESSES[api].includes(h));

/**
 * The calls of the Self-hosted model path: the body of `POST /api/subscriptions`
 * and the key to store after it (none: no `api-key` call), or the first error.
 */
export function selfHostedBody(f: SelfHostedForm): { def: NewSubscription; key?: string } | { error: string } {
  const name = f.name.trim();
  if (!name) return { error: "the name is empty" };
  const u = urlError("the base URL", f.baseUrl);
  if (u) return { error: u };
  if (f.statusUrl.trim()) {
    const e = urlError("the status page URL", f.statusUrl, true);
    if (e) return { error: e };
  }
  const ctx = parseCount("the context window", f.contextWindow, 1024);
  if ("error" in ctx) return ctx;
  const out = parseCount("the max output tokens", f.maxOutputTokens, 1);
  if ("error" in out) return out;
  if (ctx.value !== undefined && out.value !== undefined && out.value > ctx.value) return { error: "the max output tokens are more than the context window" };
  const streams = parseCount("the concurrent streams", f.maxConcurrent, 1, 1024);
  if ("error" in streams) return streams;
  const harnesses = styleHarnesses(f.api, f.harnesses);
  if (!harnesses.length) return { error: "pick at least one harness" };
  const models = parseModels(f.models);
  const sh: SelfHosted = {
    api: f.api,
    base_url: f.baseUrl.trim().replace(/\/+$/, ""),
    ...(models.length ? { models } : {}),
    ...(f.defaultModel.trim() ? { default_model: f.defaultModel.trim() } : {}),
    ...(ctx.value !== undefined ? { context_window: ctx.value } : {}),
    ...(out.value !== undefined ? { max_output_tokens: out.value } : {}),
    ...(streams.value !== undefined ? { max_concurrent: streams.value } : {}),
    ...(f.statusUrl.trim() ? { status_url: f.statusUrl.trim() } : {}),
  };
  const def: NewSubscription = { name, provider: "selfhosted", harnesses, selfhosted: sh };
  return f.key.trim() ? { def, key: f.key.trim() } : { def };
}

/** A token count in short words: `128k`, `1M`, `5000`. */
export function tokensText(n: number): string {
  if (n >= 1_048_576 && n % 1_048_576 === 0) return `${n / 1_048_576}M`;
  if (n >= 1_000_000 && n % 1_000_000 === 0) return `${n / 1_000_000}M`;
  if (n >= 1024 && n % 1024 === 0) return `${n / 1024}k`;
  if (n >= 1000 && n % 1000 === 0) return `${n / 1000}k`;
  return String(n);
}

/** `self-hosted · anthropic · 2 streams · 128k ctx` for the subscription row. */
export function selfHostedSummary(s: Pick<Subscription, "selfhosted">): string | null {
  const sh = s.selfhosted;
  if (!sh) return null;
  const parts = ["self-hosted", sh.api];
  if (sh.max_concurrent) parts.push(`${sh.max_concurrent} ${sh.max_concurrent === 1 ? "stream" : "streams"}`);
  if (sh.context_window) parts.push(`${tokensText(sh.context_window)} ctx`);
  return parts.join(" · ");
}

/** `1/2 streams in use · 1 waiting`, or null with no stream limit. */
export function streamsText(s: Pick<Subscription, "streams">): string | null {
  const st = s.streams;
  if (!st) return null;
  return `${st.in_use}/${st.max} streams in use${st.waiting > 0 ? ` · ${st.waiting} waiting` : ""}`;
}

/** The text of a stop-all: who stops, and who did not. */
export function stopAllText(name: string, r: { stopped: { project: string; name: string; error?: string }[] }, disabled: boolean): string {
  const ok = r.stopped.filter((a) => !a.error).map((a) => `${a.project}/${a.name}`);
  const bad = r.stopped.filter((a) => a.error).map((a) => `${a.project}/${a.name} (${a.error})`);
  const head = disabled ? `${name} is disabled. ` : "";
  if (!r.stopped.length) return `${head}No agent on ${name} was running.`;
  return `${head}Stopping ${ok.length ? ok.join(", ") : "no agent"}.${bad.length ? ` Not stopped: ${bad.join(", ")}.` : ""}`;
}
