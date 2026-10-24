// Add subscription wizard (§16.1). One path per kind. The login terminal is
// operator-only and torn down when the wizard closes or after ten minutes.

import { createSignal, For, Match, onCleanup, Show, Switch } from "solid-js";
import { Terminal } from "../../components/Terminal";
import { CopyButton, ErrorBanner, Modal, Tabs, useNow } from "../../components/ui";
import type { LoginKind, LoginSession, Subscription } from "../../api/types";
import { duration } from "../../lib/format";
import { api, subscriptions } from "../../stores";

export const KINDS: { kind: LoginKind; title: string; detail: string }[] = [
  { kind: "claude", title: "Claude Code", detail: "Anthropic plan. Runs claude auth login; paste the code back." },
  { kind: "codex", title: "Codex", detail: "ChatGPT plan. Device code with codex login --device-auth." },
  { kind: "copilot", title: "Copilot", detail: "GitHub plan. Device code, or a fine-grained PAT." },
  { kind: "pi_oauth", title: "pi, OAuth provider", detail: "Runs pi; use /login for the provider." },
  { kind: "pi_api_key", title: "pi, API key", detail: "Anthropic, OpenAI, Google, OpenRouter, xAI, Groq, or OpenAI-compatible." },
];

const PROVIDERS = ["anthropic", "openai", "google", "openrouter", "xai", "groq", "openai-compatible"];
const TEARDOWN_MS = 10 * 60_000;

export function AddSubscriptionWizard(props: { initialKind?: LoginKind; onClose: () => void }) {
   
  const [kind, setKind] = createSignal<LoginKind | null>(props.initialKind ?? null);
  const [done, setDone] = createSignal<Subscription | null>(null);

  const finish = async (name: string) => {
    await subscriptions.refresh();
    setDone(subscriptions.list().find((s) => s.name === name) ?? null);
  };

  const step = () => (done() ? 3 : kind() ? 2 : 1);

  return (
    <Modal title="Add subscription" onClose={() => props.onClose()}>
      <div class="steps" aria-label="Steps">
        <span class={step() === 1 ? "on" : ""}>1 Kind</span>
        <span class={step() === 2 ? "on" : ""}>2 Log in</span>
        <span class={step() === 3 ? "on" : ""}>3 Verify</span>
      </div>
      <Switch>
        <Match when={done()}>{(s) => <DoneStep sub={s()} onClose={() => props.onClose()} />}</Match>
        <Match when={kind() === "pi_api_key"}>
          <ApiKeyStep onBack={() => setKind(null)} onDone={finish} />
        </Match>
        <Match when={kind()}>{(k) => <LoginStep kind={k()} onBack={() => setKind(null)} onDone={finish} />}</Match>
        <Match when={!kind()}>
          <div class="kind-grid">
            <For each={KINDS}>
              {(k) => (
                <button onClick={() => setKind(k.kind)}>
                  <strong>{k.title}</strong>
                  <span class="muted small">{k.detail}</span>
                </button>
              )}
            </For>
          </div>
        </Match>
      </Switch>
    </Modal>
  );
}

function LoginStep(props: { kind: LoginKind; onBack: () => void; onDone: (name: string) => void }) {
  const now = useNow(1000);
  const [session, setSession] = createSignal<LoginSession | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [code, setCode] = createSignal("");
  const [provider, setProvider] = createSignal("anthropic");
  const [reuse, setReuse] = createSignal(true);
  const [tab, setTab] = createSignal<"device" | "pat">("device");
  const [startedAt, setStartedAt] = createSignal(0);
  let poll: ReturnType<typeof setInterval> | undefined;
  let teardown: ReturnType<typeof setTimeout> | undefined;

  const stop = () => {
    clearInterval(poll);
    clearTimeout(teardown);
  };
  onCleanup(stop);

  const start = async () => {
    try {
      setError(null);
      const s = await api.subscriptions.startLogin(props.kind, props.kind === "pi_oauth" ? provider() : undefined);
      setSession(s);
      setStartedAt(Date.now());
      poll = setInterval(async () => {
        try {
          const st = await api.subscriptions.loginState(s.id);
          setSession(st);
          if (st.state === "done" && st.subscription) {
            stop();
            props.onDone(st.subscription);
          }
          if (st.state === "error") {
            stop();
            setError(st.error ?? "Login failed.");
          }
        } catch (e) {
          setError((e as Error).message);
        }
      }, 1000);
      teardown = setTimeout(() => {
        stop();
        setSession(null);
        setError("The login terminal closed after ten minutes. Start again.");
      }, TEARDOWN_MS);
    } catch (e) {
      setError((e as Error).message);
    }
  };

  const sendCode = async () => {
    const s = session();
    if (!s || !code().trim()) return;
    try {
      await api.subscriptions.loginInput(s.id, code().trim());
      setCode("");
    } catch (e) {
      setError((e as Error).message);
    }
  };

  const title = () => KINDS.find((k) => k.kind === props.kind)?.title ?? props.kind;
  const deviceFlow = () => props.kind === "codex" || props.kind === "copilot";

  return (
    <div class="stack">
      <div class="spread">
        <h3 style={{ margin: 0 }}>{title()}</h3>
        <button class="small" onClick={() => props.onBack()}>
          Back
        </button>
      </div>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />

      <Show
        when={session()}
        fallback={
          <div class="stack">
            <Show when={props.kind === "pi_oauth"}>
              <label>
                Provider
                <select value={provider()} onChange={(e) => setProvider(e.currentTarget.value)}>
                  <For each={["anthropic", "openai", "google", "github-copilot"]}>{(p) => <option value={p}>{p}</option>}</For>
                </select>
              </label>
              <Show when={provider() === "anthropic" || provider() === "openai"}>
                <label class="row" style={{ display: "flex" }}>
                  <input type="checkbox" checked={reuse()} onChange={(e) => setReuse(e.currentTarget.checked)} />
                  Reuse the {provider() === "anthropic" ? "Claude Code" : "Codex"} login, so one login serves both harnesses
                </label>
              </Show>
            </Show>
            <Show when={props.kind === "claude"}>
              <p class="muted small" style={{ margin: 0 }}>
                Alternative: run <code>claude setup-token</code> for a long-lived token.
              </p>
            </Show>
            <div>
              <button class="primary" onClick={start}>
                Start login
              </button>
            </div>
          </div>
        }
      >
        {(s) => (
          <>
            <div class="spread small">
              <span>
                State: <span class={`chip ${s().state === "done" ? "ok" : s().state === "error" ? "bad" : "info"}`}>{s().state.replace(/_/g, " ")}</span>
              </span>
              <span class="muted">Terminal closes in {duration(TEARDOWN_MS - (now() - startedAt()))}</span>
            </div>

            <Show when={props.kind === "copilot"}>
              <Tabs
                tabs={[
                  { id: "device", label: "Device code" },
                  { id: "pat", label: "Fine-grained PAT" },
                ] as const}
                active={tab()}
                onChange={setTab}
              />
            </Show>

            <Show when={deviceFlow() && tab() === "device" && s().device_code}>
              <div class="stack" style={{ gap: "8px" }}>
                <div class="copy-row">
                  <span class="code-display">{s().device_code}</span>
                  <CopyButton text={s().device_code!} label="Copy code" />
                </div>
                <div class="copy-row">
                  <a href={s().url} target="_blank" rel="noopener noreferrer">
                    {s().url}
                  </a>
                  <CopyButton text={s().url!} label="Copy URL" />
                </div>
                <p class="muted small" style={{ margin: 0 }}>
                  Open the URL, enter the code, and approve. The daemon polls the login status until it succeeds.
                </p>
              </div>
            </Show>

            <Show when={props.kind === "copilot" && tab() === "pat"}>
              <label>
                Personal access token with the Copilot Requests permission
                <input type="password" value={code()} onInput={(e) => setCode(e.currentTarget.value)} autocomplete="off" />
              </label>
              <div>
                <button class="primary" onClick={sendCode} disabled={!code().trim()}>
                  Use token
                </button>
              </div>
            </Show>

            <Show when={!deviceFlow() && s().url}>
              <div class="copy-row">
                <a href={s().url} target="_blank" rel="noopener noreferrer">
                  {s().url}
                </a>
                <CopyButton text={s().url!} label="Copy URL" />
              </div>
              <Show when={props.kind === "claude"}>
                <form
                  class="row"
                  onSubmit={(e) => {
                    e.preventDefault();
                    void sendCode();
                  }}
                >
                  <input style={{ flex: 1 }} placeholder="Paste the code from the browser" value={code()} onInput={(e) => setCode(e.currentTarget.value)} autocomplete="off" aria-label="Login code" />
                  <button class="primary" type="submit" disabled={!code().trim()}>
                    Send code
                  </button>
                </form>
                <p class="muted small" style={{ margin: 0 }}>
                  The daemon types the code into the terminal and never sends it back to the browser.
                </p>
              </Show>
            </Show>

            <Terminal connect={() => api.subscriptions.loginPty(s().id)} title="Login terminal" interactive short />
          </>
        )}
      </Show>
    </div>
  );
}

function ApiKeyStep(props: { onBack: () => void; onDone: (name: string) => void }) {
  const [provider, setProvider] = createSignal("openrouter");
  const [name, setName] = createSignal("openrouter-key");
  const [key, setKey] = createSignal("");
  const [baseUrl, setBaseUrl] = createSignal("");
  const [models, setModels] = createSignal("");
  const [authJson, setAuthJson] = createSignal(true);
  const [error, setError] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);

  const submit = async (e: Event) => {
    e.preventDefault();
    setBusy(true);
    try {
      await api.subscriptions.setApiKey(name().trim(), {
        provider: provider(),
        key: key(),
        base_url: provider() === "openai-compatible" ? baseUrl() : undefined,
        models: models().trim() ? models().split(",").map((m) => m.trim()) : undefined,
      });
      setKey("");
      props.onDone(name().trim());
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <form class="stack" onSubmit={submit}>
      <div class="spread">
        <h3 style={{ margin: 0 }}>pi, API key</h3>
        <button type="button" class="small" onClick={() => props.onBack()}>
          Back
        </button>
      </div>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <div class="grid" style={{ "grid-template-columns": "repeat(auto-fill, minmax(min(100%, 220px), 1fr))" }}>
        <label>
          Provider
          <select
            value={provider()}
            onChange={(e) => {
              setProvider(e.currentTarget.value);
              setName(`${e.currentTarget.value}-key`);
            }}
          >
            <For each={PROVIDERS}>{(p) => <option value={p}>{p}</option>}</For>
          </select>
        </label>
        <label>
          Subscription name
          <input value={name()} onInput={(e) => setName(e.currentTarget.value)} required />
        </label>
      </div>
      <label>
        API key
        <input type="password" value={key()} onInput={(e) => setKey(e.currentTarget.value)} autocomplete="off" required />
      </label>
      <Show when={provider() === "openai-compatible"}>
        <label>
          Base URL
          <input type="url" value={baseUrl()} onInput={(e) => setBaseUrl(e.currentTarget.value)} placeholder="https://llm.example.com/v1" required />
        </label>
      </Show>
      <label>
        Models (comma separated, optional)
        <input value={models()} onInput={(e) => setModels(e.currentTarget.value)} />
      </label>
      <label class="row" style={{ display: "flex" }}>
        <input type="checkbox" checked={authJson()} onChange={(e) => setAuthJson(e.currentTarget.checked)} />
        Also write it to pi's auth.json for pi auth compatibility
      </label>
      <p class="muted small" style={{ margin: 0 }}>
        The daemon stores the key under /state/credentials with mode 0600. Nobody can read it back.
      </p>
      <div>
        <button class="primary" type="submit" disabled={busy() || !key() || !name().trim()}>
          Save key
        </button>
      </div>
    </form>
  );
}

function DoneStep(props: { sub: Subscription; onClose: () => void }) {
  return (
    <div class="stack">
      <div class="notice">
        Logged in. The daemon created the subscription <strong>{props.sub.name}</strong>.
      </div>
      <dl class="kv">
        <dt>Provider</dt>
        <dd>{props.sub.provider}</dd>
        <dt>Plan</dt>
        <dd>{props.sub.plan}</dd>
        <dt>Harnesses</dt>
        <dd>{props.sub.harnesses.join(", ")}</dd>
        <dt>Probe</dt>
        <dd>{props.sub.probe ?? "none: the tile shows a local estimate only"}</dd>
        <dt>First quota</dt>
        <dd>
          <Show when={props.sub.windows.length} fallback={<span class="muted">no samples</span>}>
            {props.sub.windows.map((w) => `${w.window} ${w.used_pct}%`).join(", ")}
          </Show>
        </dd>
      </dl>
      <div>
        <button class="primary" onClick={() => props.onClose()}>
          Done
        </button>
      </div>
    </div>
  );
}
