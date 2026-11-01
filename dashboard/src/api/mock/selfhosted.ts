// Self-hosted model servers in the mock (docs/api.md §16). The daemon side
// is `terrarium_core::selfhosted`, `terrarium_core::providers`, and
// `api/subscriptions.rs`. The rules here follow the daemon's.

import type { FetchModelsInput, FetchModelsResult, Harness, SelfHosted, SelfHostedApi, Subscription } from "../types";

export const SELFHOSTED = "selfhosted";
/** The old provider word: an alias of `selfhosted` with the OpenAI style. */
export const OPENAI_COMPATIBLE = "openai-compatible";
export const SELFHOSTED_KEY_ENV = "TERRARIUM_SELFHOSTED_API_KEY";

export const isSelfHosted = (provider: string | undefined): boolean => {
  const p = (provider ?? "").trim().toLowerCase();
  return p === SELFHOSTED || p === OPENAI_COMPATIBLE;
};

/** The harnesses of each API style. Copilot never. */
export const STYLE_HARNESSES: Record<SelfHostedApi, Harness[]> = { openai: ["pi", "codex"], anthropic: ["claude", "pi"] };

/** The public status pages (`terrarium_core::providers::status_url`). Z.ai has none. */
const STATUS: Record<string, string> = {
  anthropic: "https://status.anthropic.com",
  claude: "https://status.anthropic.com",
  openai: "https://status.openai.com",
  "openai-codex": "https://status.openai.com",
  codex: "https://status.openai.com",
  chatgpt: "https://status.openai.com",
  github: "https://www.githubstatus.com",
  "github-copilot": "https://www.githubstatus.com",
  copilot: "https://www.githubstatus.com",
  openrouter: "https://status.openrouter.ai",
  moonshot: "https://status.moonshot.cn",
};

export function statusUrlOf(provider: string, sh?: SelfHosted): string | undefined {
  if (isSelfHosted(provider)) return sh?.status_url;
  return STATUS[provider.trim().toLowerCase()];
}

/** `check_http_url` of the daemon: the clean URL, or an error text. */
export function checkHttpUrl(what: string, url: string, allowQuery: boolean): { url: string } | { error: string } {
  const u = url.trim().replace(/\/+$/, "");
  if (!u) return { error: `${what} is empty` };
  if (/[\s\p{Cc}]/u.test(u)) return { error: `${what} has white space or a control character` };
  const m = /^(https?):\/\/(.*)$/i.exec(u);
  if (!m) return { error: `${what} must start with http:// or https://` };
  const rest = m[2]!;
  if (!allowQuery && /[?#]/.test(rest)) return { error: `${what} must not have a query (\`?\`) or a fragment (\`#\`)` };
  const authority = rest.split(/[/?#]/)[0] ?? "";
  if (authority.includes("@")) return { error: `${what} must not hold a user or a password; store the key with the API key instead` };
  const hp = /^(\[[^\]]*\]|[^:]*)(?::(.*))?$/.exec(authority);
  if (!hp?.[1] || hp[1] === "[]") return { error: `${what} has no host` };
  const port = hp[2];
  if (port !== undefined && (!/^\d{1,5}$/.test(port) || Number(port) > 65535)) return { error: `${what} has a bad port \`${port}\`` };
  return { url: u };
}

/** `SelfHosted::checked`: the stored form, or an error text. */
export function checkSelfHosted(sh: SelfHosted): { sh: SelfHosted } | { error: string } {
  const base = checkHttpUrl("base_url", sh.base_url ?? "", false);
  if ("error" in base) return base;
  const models = [...new Set((sh.models ?? []).map((m) => m.trim()).filter(Boolean))];
  const out: SelfHosted = { api: sh.api ?? "openai", base_url: base.url, ...(models.length ? { models } : {}) };
  const def = sh.default_model?.trim();
  if (def) out.default_model = def;
  if (sh.context_window != null) {
    if (sh.context_window < 1024) return { error: `context_window is ${sh.context_window}; it is a token count of at least 1024` };
    out.context_window = sh.context_window;
  }
  if (sh.max_output_tokens != null) {
    if (sh.max_output_tokens < 1) return { error: "max_output_tokens must be at least 1" };
    if (out.context_window != null && sh.max_output_tokens > out.context_window) return { error: `max_output_tokens (${sh.max_output_tokens}) is larger than context_window (${out.context_window})` };
    out.max_output_tokens = sh.max_output_tokens;
  }
  if (sh.max_concurrent != null) {
    if (sh.max_concurrent < 1 || sh.max_concurrent > 1024) return { error: `max_concurrent is ${sh.max_concurrent}; use 1 to 1024, or leave it out for no limit` };
    out.max_concurrent = sh.max_concurrent;
  }
  if (sh.status_url?.trim()) {
    const st = checkHttpUrl("status_url", sh.status_url, true);
    if ("error" in st) return st;
    out.status_url = st.url;
  }
  return { sh: out };
}

/** Why `harness` cannot spend a server of `api`, or null. */
export function styleRefusal(api: SelfHostedApi, harness: Harness): string | null {
  return STYLE_HARNESSES[api].includes(harness) ? null : `a self-hosted subscription with the ${api} API is for the ${STYLE_HARNESSES[api].join(" and ")} harnesses, not ${harness}`;
}

/** The harnesses of a new self-hosted subscription (none given: all of the style), or an error text. */
export function selfHostedHarnesses(name: string, api: SelfHostedApi, harnesses: Harness[] | undefined): { harnesses: Harness[] } | { error: string } {
  const list = harnesses?.length ? harnesses : STYLE_HARNESSES[api];
  for (const h of list) {
    const why = styleRefusal(api, h);
    if (why) return { error: `subscription \`${name}\`: ${why}` };
  }
  return { harnesses: [...list] };
}

/** Why `harness` cannot spend `s` (a self-hosted style refusal), or null. */
export function harnessRefusal(s: Subscription, harness: Harness): string | null {
  if (!isSelfHosted(s.provider)) return null;
  const why = styleRefusal(s.selfhosted?.api ?? "openai", harness);
  return why ? `subscription \`${s.name}\`: ${why}` : null;
}

/** The models that "Fetch models" finds on any mock server. */
export const FETCHED_MODELS = ["qwen3-coder-30b", "glm-5.3-air", "gpt-oss-120b"];

/** `POST /api/subscriptions/fetch-models`. A URL with `fail` in it does not answer. */
export function fetchModels(input: FetchModelsInput, stored: SelfHosted | undefined, at: string): { result: FetchModelsResult } | { status: number; code: string; error: string } {
  const api = input.api ?? stored?.api ?? "openai";
  const raw = input.base_url ?? stored?.base_url;
  if (!raw) return { status: 422, code: "invalid", error: "give `base_url` (or `subscription`)" };
  const base = checkHttpUrl("base_url", raw, false);
  if ("error" in base) return { status: 422, code: "invalid", error: base.error };
  if (stored && input.base_url && base.url !== stored.base_url)
    return { status: 422, code: "invalid", error: "a fetch with the stored key goes to the stored base_url only; leave `base_url` out, or leave `subscription` out and give the key" };
  const url = api === "anthropic" ? `${base.url}/v1/models` : `${base.url}/models`;
  if (base.url.includes("fail")) return { status: 503, code: "unavailable", error: `GET ${url}: error sending request: connection refused` };
  return { result: { api, base_url: base.url, models: FETCHED_MODELS.map((id) => ({ id })), cached: false, fetched_at: at } };
}
