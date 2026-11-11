import { InstanceAnchor } from "../../instance/navigation";
import { useInstance } from "../../instance";
// Add subscription wizard (PLAN §16.1, docs/api.md §16.4). One path per kind.
// The state machine is in ./wizard.ts. The login terminal is operator-only;
// the daemon tears it down on completion or after ten minutes. Closing the
// wizard cancels a live session (`DELETE /api/subscriptions/login/{id}`).

import { createEffect, createResource, createSignal, For, Match, on, onCleanup, Show, Switch } from "solid-js";
import { ApiError } from "../../api/http";
import { HARNESSES, type Harness, type LoginSession } from "../../api/types";
import { Terminal } from "../../components/Terminal";
import { isLocalOnly } from "../../components/UsageTile";
import { CopyButton, ErrorBanner, Modal, useNow } from "../../components/ui";
import { humanize, orderWindows, subscriptionTitle, windowLabel } from "../../lib/format";

import {
  apiKeyBody,
  countdown,
  defaultPlan,
  initialWizard,
  isDeviceFlow,
  isLoginPath,
  isSelfHosted,
  KEY_HELP,
  KEY_PLANS,
  KEY_PROVIDERS,
  sessionToCancel,
  shouldPoll,
  takesCode,
  takesPlan,
  teardownLeft,
  wizardReduce,
  emptySelfHostedForm,
  parseModels,
  selfHostedBody,
  STYLE_HARNESSES,
  STYLES,
  styleHarnesses,
  type SelfHostedForm,
  type WizardAction,
  type WizardLogin,
  type WizardPath,
  type WizardSetup,
  type WizardState,
} from "./wizard";

export const PATHS: { path: WizardPath; title: string; detail: string; setup?: WizardSetup }[] = [
  { path: "api_key", setup: { provider: "litellm" }, title: "LiteLLM", detail: "Claude Code through your LiteLLM proxy: HTTPS base URL, key, and models." },
  { path: "claude", title: "Claude Code", detail: "Anthropic plan. Runs claude auth login; paste the code back." },
  { path: "codex", title: "Codex", detail: "ChatGPT plan. Device code with codex login --device-auth." },
  { path: "copilot", title: "Copilot", detail: "GitHub plan. Device code with copilot login --device-code." },
  { path: "api_key", title: "API key", detail: "Amazon Bedrock or Google Cloud Vertex AI (Claude Code), Anthropic, OpenAI, Google, OpenRouter, xAI, Groq, Z.ai (GLM), Moonshot, or a GitHub PAT. pi uses API keys only." },
  { path: "selfhosted", title: "Self-hosted model", detail: "Your own model server (vLLM, Ollama, llama.cpp, SGLang): OpenAI or Anthropic API style, the models, and the limits." },
  { path: "define", title: "Define only", detail: "Write the definition now; log in or add a key later." },
];

const PROBES = ["anthropic-oauth-usage", "openai-codex-usage", "github-copilot-quota"];
const POLL_MS = 1000;

const STATE_CHIP: Record<LoginSession["state"], string> = {
  starting: "info",
  waiting: "info",
  verifying: "info",
  done: "ok",
  failed: "bad",
  cancelled: "",
  expired: "bad",
};

export function AddSubscriptionWizard(props: { path?: WizardPath; setup?: WizardSetup; onClose: () => void }) {
  const { api, subscriptions } = useInstance();
  const [state, setState] = createSignal<WizardState>(initialWizard(props.path, props.setup));
  const [error, setError] = createSignal<unknown>(null);
  const send = (a: WizardAction) => setState((s) => wizardReduce(s, a));

  // Poll the session while it is live.
  createEffect(
    on(
      () => (shouldPoll(state()) && state().step === "live" ? (state() as Extract<WizardState, { step: "live" }>).session.id : null),
      (id) => {
        if (!id) return;
        const t = setInterval(async () => {
          try {
            send({ type: "polled", session: await api.subscriptions.loginState(id) });
          } catch (e) {
            setError(e);
            const s = state();
            // The session is gone (a daemon without it): end the flow.
            if (e instanceof ApiError && e.status === 404 && s.step === "live") send({ type: "polled", session: { ...s.session, state: "expired", done: true } });
          }
        }, POLL_MS);
        onCleanup(() => clearInterval(t));
      },
    ),
  );

  // A done login made or updated a subscription: show it in the list.
  createEffect(on(() => (state().step === "done" ? (state() as Extract<WizardState, { step: "done" }>).subscription : null), (name) => name && void subscriptions.refresh()));

  const cancelLive = () => {
    const id = sessionToCancel(state());
    // `409 not_running`: it ended by itself in the meantime.
    if (id) void api.subscriptions.cancelLogin(id).catch(() => undefined);
  };
  onCleanup(cancelLive);
  const close = () => {
    cancelLive();
    props.onClose();
  };

  const start = async () => {
    const s = state();
    if (s.step !== "setup" || !isLoginPath(s.path)) return;
    send({ type: "start" });
    setError(null);
    try {
      const session = await api.subscriptions.startLogin({
        kind: s.path,
        ...(s.setup.name?.trim() ? { name: s.setup.name.trim() } : {}),
      });
      send({ type: "started", session });
    } catch (e) {
      setError(e);
      send({ type: "failed" });
    }
  };

  const step = () => {
    const s = state().step;
    return s === "done" ? 3 : s === "pick" ? 1 : 2;
  };

  return (
    <Modal title="Add subscription" onClose={close}>
      <div class="steps" aria-label="Steps">
        <span class={step() === 1 ? "on" : ""}>1 Kind</span>
        <span class={step() === 2 ? "on" : ""}>2 Log in</span>
        <span class={step() === 3 ? "on" : ""}>3 Verify</span>
      </div>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Switch>
        <Match when={state().step === "pick"}>
          <div class="kind-grid">
            <For each={PATHS}>
              {(k) => (
                <button onClick={() => send({ type: "pick", path: k.path, setup: k.setup })}>
                  <strong>{k.title}</strong>
                  <span class="muted small">{k.detail}</span>
                </button>
              )}
            </For>
          </div>
        </Match>
        <Match when={state().step === "setup" && (state() as Extract<WizardState, { step: "setup" }>)}>
          {(s) => (
            <Switch>
              <Match when={s().path === "api_key"}>
                <ApiKeyForm setup={s().setup} onBack={() => send({ type: "back" })} onSaved={(name) => send({ type: "saved", subscription: name })} />
              </Match>
              <Match when={s().path === "selfhosted"}>
                <SelfHostedSetup onBack={() => send({ type: "back" })} onSaved={(name) => send({ type: "saved", subscription: name })} />
              </Match>
              <Match when={s().path === "define"}>
                <DefineForm onBack={() => send({ type: "back" })} onSaved={(name) => send({ type: "saved", subscription: name })} />
              </Match>
              <Match when={isLoginPath(s().path) && (s().path as WizardLogin)}>
                {(kind) => <LoginSetup kind={kind()} setup={s().setup} onSetup={(setup) => send({ type: "setup", setup })} onBack={() => send({ type: "back" })} onStart={start} onPat={() => setState({ step: "setup", path: "api_key", setup: { provider: "github", name: s().setup.name } })} />}
              </Match>
            </Switch>
          )}
        </Match>
        <Match when={state().step === "starting"}>
          <p class="muted">Starting the login terminal…</p>
        </Match>
        <Match when={state().step === "live" && (state() as Extract<WizardState, { step: "live" }>)}>
          {(s) => <LiveLogin kind={s().path} session={s().session} onError={setError} onCancel={async () => {
            try {
              await api.subscriptions.cancelLogin(s().session.id);
              send({ type: "polled", session: await api.subscriptions.loginState(s().session.id) });
            } catch (e) {
              setError(e);
            }
          }} />}
        </Match>
        <Match when={state().step === "ended" && (state() as Extract<WizardState, { step: "ended" }>)}>
          {(s) => (
            <div class="stack">
              <div class="notice">{s().message}</div>
              <div class="row">
                <button class="primary" onClick={() => send({ type: "retry" })}>
                  Try again
                </button>
                <button onClick={() => send({ type: "back" })}>Choose another kind</button>
              </div>
            </div>
          )}
        </Match>
        <Match when={state().step === "done" && (state() as Extract<WizardState, { step: "done" }>)}>
          {(s) => <DoneStep name={s().subscription} onClose={close} />}
        </Match>
      </Switch>
    </Modal>
  );
}

function LoginSetup(props: { kind: WizardLogin; setup: WizardSetup; onSetup: (s: WizardSetup) => void; onBack: () => void; onStart: () => void; onPat: () => void }) {
  const title = () => PATHS.find((k) => k.path === props.kind)?.title ?? props.kind;
  return (
    <form
      class="stack"
      onSubmit={(e) => {
        e.preventDefault();
        props.onStart();
      }}
    >
      <div class="spread">
        <h3 style={{ margin: 0 }}>{title()}</h3>
        <button type="button" class="small" onClick={() => props.onBack()}>
          Back
        </button>
      </div>
      <label>
        Subscription name (optional)
        <input value={props.setup.name ?? ""} onInput={(e) => props.onSetup({ name: e.currentTarget.value })} placeholder="from the status output" autocomplete="off" />
      </label>
      <p class="muted small" style={{ margin: 0 }}>
        A second login of the same provider makes a second account: a new subscription, for example <code>anthropic-max-20x-2</code>. Re-login on a card replaces the login of that account only.
      </p>
      <p class="muted small" style={{ margin: 0 }}>
        The daemon runs the login command in a terminal as its own user, with no sandbox, against the shared harness home. It is open to the operator only and closes after ten minutes.
      </p>
      <Show when={props.kind === "claude"}>
        <p class="muted small" style={{ margin: 0 }}>
          Alternative: run <code>claude setup-token</code> for a long-lived token, then store it as an <strong>API key</strong> for provider anthropic.
        </p>
      </Show>
      <Show when={props.kind === "copilot"}>
        <p class="muted small" style={{ margin: 0 }}>
          Or use a fine-grained PAT with the Copilot Requests permission:{" "}
          <button type="button" class="small" onClick={() => props.onPat()}>
            Use a PAT
          </button>
        </p>
      </Show>
      <div>
        <button class="primary" type="submit">
          Start login
        </button>
      </div>
    </form>
  );
}

function LiveLogin(props: { kind: WizardLogin; session: LoginSession; onError: (e: unknown) => void; onCancel: () => void }) {
  const { api } = useInstance();
  const now = useNow(1000);
  const [code, setCode] = createSignal("");
  const [sent, setSent] = createSignal(false);
  const left = () => teardownLeft(props.session, now());

  const sendCode = async () => {
    const c = code().trim();
    if (!c) return;
    try {
      await api.subscriptions.loginInput(props.session.id, c);
      setCode("");
      setSent(true);
    } catch (e) {
      props.onError(e);
    }
  };

  return (
    <div class="stack">
      <div class="spread small">
        <span class="row">
          State: <span class={`chip ${STATE_CHIP[props.session.state]}`}>{humanize(props.session.state)}</span>
        </span>
        <span class={left() < 60_000 ? "chip warn" : "muted"} title="The daemon tears the login terminal down after ten minutes">
          Terminal closes in {countdown(left())}
        </span>
      </div>
      <Show when={props.session.instructions}>
        <p style={{ margin: 0 }}>{props.session.instructions}</p>
      </Show>

      <Show when={isDeviceFlow(props.kind) && props.session.device_code}>
        <div class="copy-row">
          <span class="code-display" aria-label="Device code">
            {props.session.device_code}
          </span>
          <CopyButton text={props.session.device_code!} label="Copy code" />
        </div>
      </Show>
      <Show when={props.session.url} fallback={<p class="muted small">Waiting for the login URL in the terminal…</p>}>
        <div class="copy-row">
          <InstanceAnchor href={props.session.url} target="_blank" rel="noopener noreferrer" class="ellipsis">
            {props.session.url}
          </InstanceAnchor>
          <CopyButton text={props.session.url!} label="Copy URL" />
        </div>
      </Show>
      <Show when={isDeviceFlow(props.kind)}>
        <p class="muted small" style={{ margin: 0 }}>
          Open the URL, enter the code, and approve. The wizard follows the session until the daemon verifies the login.
        </p>
      </Show>

      <Show when={takesCode(props.kind)}>
        <form
          class="row"
          onSubmit={(e) => {
            e.preventDefault();
            void sendCode();
          }}
        >
          <input
            type="password"
            style={{ flex: 1 }}
            placeholder="Paste the code from the browser"
            value={code()}
            onInput={(e) => setCode(e.currentTarget.value)}
            autocomplete="off"
            aria-label="Login code"
          />
          <button class="primary" type="submit" disabled={!code().trim()}>
            Send code
          </button>
        </form>
        <p class="muted small" style={{ margin: 0 }}>
          {sent() ? "Code sent. " : ""}The daemon types the code into the terminal and never stores it or sends it back.
        </p>
      </Show>

      <Terminal connect={() => api.subscriptions.loginPty(props.session.id)} available={!props.session.done} title="Login terminal" interactive short />

      <div>
        <button class="danger" onClick={() => props.onCancel()}>
          Cancel login
        </button>
      </div>
    </div>
  );
}

function ApiKeyForm(props: { setup: WizardSetup; onBack: () => void; onSaved: (name: string) => void }) {
  const { api } = useInstance();
  const [provider, setProvider] = createSignal(props.setup.provider ?? "openrouter");
  const [name, setName] = createSignal(props.setup.name ?? `${props.setup.provider ?? "openrouter"}-key`);
  const [key, setKey] = createSignal("");
  const [keyId, setKeyId] = createSignal("");
  const [region, setRegion] = createSignal(props.setup.provider === "gcp" ? "us-east5" : "us-east-1");
  const [projectId, setProjectId] = createSignal("");
  const [cloudModels, setCloudModels] = createSignal<Record<string, string>>({});
  const [baseUrl, setBaseUrl] = createSignal("");
  const [plan, setPlan] = createSignal(defaultPlan(props.setup.provider ?? "openrouter"));
  const [models, setModels] = createSignal("");
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const fixedName = !!props.setup.name;

  const submit = async (e: Event) => {
    e.preventDefault();
    setBusy(true);
    try {
      const n = name().trim();
      await api.subscriptions.setApiKey(n, apiKeyBody({ provider: provider(), key: key(), plan: plan(), baseUrl: baseUrl(), models: models(), keyId: keyId(), region: region(), projectId: projectId(), cloudModels: cloudModels() }));
      setKey("");
      setKeyId("");
      props.onSaved(n);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };

  const env = () => KEY_PROVIDERS.find((p) => p.id === provider())?.env;
  const harnesses = () => KEY_PROVIDERS.find((p) => p.id === provider())?.harnesses;

  return (
    <form class="stack" onSubmit={submit}>
      <div class="spread">
        <h3 style={{ margin: 0 }}>API key</h3>
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
              setKey("");
              setKeyId("");
              setRegion(e.currentTarget.value === "gcp" ? "us-east5" : "us-east-1");
              setProjectId("");
              setCloudModels({});
              setPlan(defaultPlan(e.currentTarget.value));
              if (!fixedName) setName(`${e.currentTarget.value}-key`);
            }}
          >
            <For each={KEY_PROVIDERS}>{(p) => <option value={p.id}>{p.label}</option>}</For>
          </select>
        </label>
        <label>
          Subscription name
          <input value={name()} onInput={(e) => setName(e.currentTarget.value)} required disabled={fixedName} />
        </label>
      </div>
      <Show when={provider() === "gcp"} fallback={<label>
        {provider() === "bedrock" ? "AWS secret access key" : provider() === "github" ? "Fine-grained PAT (Copilot Requests permission)" : "API key"}
        <input type="password" value={key()} onInput={(e) => setKey(e.currentTarget.value)} autocomplete="off" required />
      </label>}>
        <label>Service-account JSON file
          <input type="file" accept=".json,application/json" onChange={async (e) => {
            const file = e.currentTarget.files?.[0];
            if (!file) return;
            try {
              if (file.size > 65536) throw new Error("The service-account JSON file is too large.");
              const text = await file.text();
              if (provider() === "gcp") setKey(text);
            } catch (err) { setError(err); }
          }} />
        </label>
        <label>Service-account JSON (or paste here)
          <textarea value={key()} onInput={(e) => setKey(e.currentTarget.value)} autocomplete="off" spellcheck={false} rows={4} required />
        </label>
        <label>Google Cloud project ID
          <input value={projectId()} onInput={(e) => setProjectId(e.currentTarget.value)} required />
        </label>
        <label>Vertex region
          <input value={region()} onInput={(e) => setRegion(e.currentTarget.value)} placeholder="us-east5, global, us, or eu" required />
        </label>
        <For each={["opus", "sonnet", "haiku"]}>{(tier) => (
          <label>{tier} Vertex model ID (optional override)
            <input value={cloudModels()[tier] ?? ""} onInput={(e) => setCloudModels({ ...cloudModels(), [tier]: e.currentTarget.value })} />
          </label>
        )}</For>
      </Show>
      <Show when={provider() === "bedrock"}>
        <label>AWS access key ID
          <input type="password" value={keyId()} onInput={(e) => setKeyId(e.currentTarget.value)} autocomplete="off" required />
        </label>
        <label>AWS region
          <input value={region()} onInput={(e) => setRegion(e.currentTarget.value)} placeholder="us-east-1" required />
        </label>
        <For each={["opus", "sonnet", "haiku"]}>{(tier) => (
          <label>{tier} inference profile (optional override)
            <input value={cloudModels()[tier] ?? ""} onInput={(e) => setCloudModels({ ...cloudModels(), [tier]: e.currentTarget.value })} />
          </label>
        )}</For>
      </Show>
      <Show when={takesPlan(provider())}>
        <label>
          Plan
          <select value={plan()} onChange={(e) => setPlan(e.currentTarget.value)}>
            <For each={KEY_PLANS[provider()] ?? []}>{(p) => <option value={p.id}>{p.label}</option>}</For>
          </select>
        </label>
      </Show>
      <Show when={KEY_HELP[provider()]}>
        <p class="muted small" style={{ margin: 0 }}>
          {KEY_HELP[provider()]}
        </p>
      </Show>
      <Show when={(isSelfHosted(provider()) || provider() === "litellm") && !fixedName}>
        <label>
          {provider() === "litellm" ? "LiteLLM HTTPS base URL (Anthropic root, without /v1)" : "Base URL (a new subscription: an OpenAI-style server)"}
          <input type="url" value={baseUrl()} onInput={(e) => setBaseUrl(e.currentTarget.value)} placeholder={provider() === "litellm" ? "https://litellm.example.com" : "http://gpu.lan:8000/v1"} required />
        </label>
      </Show>
      <Show when={provider() !== "bedrock"}><label>
        Models (comma separated, optional)
        <input value={models()} onInput={(e) => setModels(e.currentTarget.value)} />
      </label></Show>
      <p class="muted small" style={{ margin: 0 }}>
        The daemon stores the key with mode 0600 and gives it to the agents on this subscription as <code>{env()}</code>. No endpoint returns it, and this page never shows it again.
      </p>
      <Show when={harnesses()}>
        {(h) => (
          <p class="muted small" style={{ margin: 0 }}>
            Harnesses: {h().join(", ")}. The daemon refuses an agent of another harness on this subscription.
          </p>
        )}
      </Show>
      <div>
        <button class="primary" type="submit" disabled={busy() || !key() || !name().trim()}>
          {provider() === "bedrock" ? busy() ? "Testing Bedrock…" : "Test and save key" : "Save key"}
        </button>
      </div>
    </form>
  );
}

function DefineForm(props: { onBack: () => void; onSaved: (name: string) => void }) {
  const { api } = useInstance();
  const [name, setName] = createSignal("");
  const [provider, setProvider] = createSignal("anthropic");
  const [plan, setPlan] = createSignal("");
  const [harnesses, setHarnesses] = createSignal<Harness[]>(["claude"]);
  const [probe, setProbe] = createSignal("");
  const [alertAt, setAlertAt] = createSignal(80);
  const [error, setError] = createSignal<unknown>(null);

  const submit = async (e: Event) => {
    e.preventDefault();
    try {
      const s = await api.subscriptions.create({
        name: name().trim(),
        provider: provider().trim(),
        harnesses: harnesses(),
        alert_at: alertAt(),
        ...(plan().trim() ? { plan: plan().trim() } : {}),
        ...(probe() ? { probe: probe() } : {}),
      });
      props.onSaved(s.name);
    } catch (err) {
      setError(err);
    }
  };

  return (
    <form class="stack" onSubmit={submit}>
      <div class="spread">
        <h3 style={{ margin: 0 }}>Define a subscription</h3>
        <button type="button" class="small" onClick={() => props.onBack()}>
          Back
        </button>
      </div>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <div class="grid" style={{ "grid-template-columns": "repeat(auto-fill, minmax(min(100%, 200px), 1fr))" }}>
        <label>
          Name
          <input value={name()} onInput={(e) => setName(e.currentTarget.value)} required placeholder="anthropic-max" />
        </label>
        <label>
          Provider
          <input value={provider()} onInput={(e) => setProvider(e.currentTarget.value)} required />
        </label>
        <label>
          Plan (optional)
          <input value={plan()} onInput={(e) => setPlan(e.currentTarget.value)} placeholder="Max 20x" />
        </label>
        <label>
          Probe
          <select value={probe()} onChange={(e) => setProbe(e.currentTarget.value)}>
            <option value="">none (local estimate only)</option>
            <For each={PROBES}>{(p) => <option value={p}>{p}</option>}</For>
          </select>
        </label>
        <label>
          Alert at (%)
          <input type="number" min="1" max="100" value={alertAt()} onInput={(e) => setAlertAt(Number(e.currentTarget.value) || 80)} />
        </label>
      </div>
      <fieldset class="row" style={{ "flex-wrap": "wrap", border: "none", padding: 0 }}>
        <legend class="small">Harnesses that can spend it</legend>
        <For each={HARNESSES}>
          {(h) => (
            <label class="row small">
              <input type="checkbox" checked={harnesses().includes(h)} onChange={(e) => setHarnesses((x) => (e.currentTarget.checked ? [...x, h] : x.filter((y) => y !== h)))} />
              {h}
            </label>
          )}
        </For>
      </fieldset>
      <div>
        <button class="primary" type="submit" disabled={!name().trim() || !provider().trim()}>
          Create definition
        </button>
      </div>
    </form>
  );
}

function DoneStep(props: { name: string; onClose: () => void }) {
  const { api } = useInstance();
  const [sub] = createResource(() => props.name, (n) => api.subscriptions.get(n));
  return (
    <div class="stack">
      <div class="notice">
        Done. The daemon has the subscription <strong>{props.name}</strong>.
      </div>
      <ErrorBanner error={sub.error} />
      <Show when={sub()}>
        {(s) => (
          <dl class="kv">
            <dt>Name</dt>
            <dd>
              {subscriptionTitle(s())} <span class="muted small mono">{s().name}</span>
            </dd>
            <dt>Provider</dt>
            <dd>{s().provider}</dd>
            <dt>Plan</dt>
            <dd>{s().plan ?? <span class="muted">not known</span>}</dd>
            <dt>Harnesses</dt>
            <dd>{s().harnesses.join(", ") || <span class="muted">none</span>}</dd>
            <dt>Login</dt>
            <dd>{humanize(s().login_status)}</dd>
            <dt>Probe</dt>
            <dd>
              {s().probe ?? "none"} · {s().probe_status}
              <Show when={s().probe_error}> ({s().probe_error})</Show>
            </dd>
            <dt>First quota</dt>
            <dd>
              <Show when={!isLocalOnly(s())} fallback={<span class="muted">Local estimate only: the tile shows local accounting.</span>}>
                <Show when={s().windows.length} fallback={<span class="muted">no samples</span>}>
                  {orderWindows(s().windows).map((w) => `${windowLabel(w.window)} ${Math.round(w.used_pct)}%`).join(", ")}
                </Show>
              </Show>
            </dd>
          </dl>
        )}
      </Show>
      <div>
        <button class="primary" onClick={() => props.onClose()}>
          Close
        </button>
      </div>
    </div>
  );
}

/**
 * The Self-hosted model path: the API style, the server, an optional key,
 * the models (fetched from the server or typed), and the limits. It makes
 * the subscription (`POST /api/subscriptions`), then stores the key when
 * the form has one (`POST .../api-key`).
 */
function SelfHostedSetup(props: { onBack: () => void; onSaved: (name: string) => void }) {
  const { api } = useInstance();
  const [f, setF] = createSignal<SelfHostedForm>(emptySelfHostedForm());
  const [error, setError] = createSignal<unknown>(null);
  const [fetchNote, setFetchNote] = createSignal<string | null>(null);
  const [fetched, setFetched] = createSignal<string[]>([]);
  const [busy, setBusy] = createSignal(false);
  const set = (p: Partial<SelfHostedForm>) => setF((x) => ({ ...x, ...p }));
  const style = () => STYLES.find((x) => x.id === f().api)!;
  const models = () => parseModels(f().models);

  const fetchModels = async () => {
    setBusy(true);
    setFetchNote(null);
    try {
      const r = await api.subscriptions.fetchModels({ api: f().api, base_url: f().baseUrl.trim(), ...(f().key.trim() ? { key: f().key.trim() } : {}) });
      const ids = r.models.map((m) => m.id);
      setFetched(ids);
      set({ models: ids.join(", "), ...(f().defaultModel || !ids[0] ? {} : { defaultModel: ids[0] }) });
      setFetchNote(`${ids.length} models from ${r.base_url}${r.cached ? " (cached)" : ""}.`);
    } catch (e) {
      setFetched([]);
      setFetchNote(`Fetch failed: ${e instanceof Error ? e.message : String(e)}. Type the model ids instead.`);
    } finally {
      setBusy(false);
    }
  };

  const submit = async (e: Event) => {
    e.preventDefault();
    const b = selfHostedBody(f());
    if ("error" in b) {
      setError(new Error(b.error));
      return;
    }
    setBusy(true);
    try {
      const s = await api.subscriptions.create(b.def);
      if (b.key) await api.subscriptions.setApiKey(s.name, { provider: "selfhosted", key: b.key });
      set({ key: "" });
      props.onSaved(s.name);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };

  return (
    <form class="stack" onSubmit={submit} aria-label="Self-hosted model">
      <div class="spread">
        <h3 style={{ margin: 0 }}>Self-hosted model</h3>
        <button type="button" class="small" onClick={() => props.onBack()}>
          Back
        </button>
      </div>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <fieldset class="row" style={{ "flex-wrap": "wrap", border: "none", padding: 0 }}>
        <legend class="small">API style</legend>
        <For each={STYLES}>
          {(st) => (
            <label class="row small">
              <input type="radio" name="selfhosted-api" checked={f().api === st.id} onChange={() => set({ api: st.id, harnesses: [...STYLE_HARNESSES[st.id]] })} />
              {st.label}
            </label>
          )}
        </For>
      </fieldset>
      <div class="grid" style={{ "grid-template-columns": "repeat(auto-fill, minmax(min(100%, 220px), 1fr))" }}>
        <label>
          Subscription name
          <input value={f().name} onInput={(e) => set({ name: e.currentTarget.value })} required placeholder="gpu-lan" autocomplete="off" />
        </label>
        <label>
          Base URL
          <input type="url" value={f().baseUrl} onInput={(e) => set({ baseUrl: e.currentTarget.value })} required placeholder={style().placeholder} />
        </label>
        <label>
          API key (optional)
          <input type="password" value={f().key} onInput={(e) => set({ key: e.currentTarget.value })} autocomplete="off" placeholder="many local servers need none" />
        </label>
      </div>
      <p class="muted small" style={{ margin: 0 }}>
        {style().hint}
      </p>
      <div class="row" style={{ "flex-wrap": "wrap", "align-items": "flex-end" }}>
        <label style={{ flex: 1, "min-width": "min(100%, 260px)" }}>
          Models (comma or line separated)
          <textarea rows={2} value={f().models} onInput={(e) => set({ models: e.currentTarget.value })} placeholder="qwen3-coder-30b, glm-5.3-air" />
        </label>
        <button type="button" disabled={busy() || !f().baseUrl.trim()} onClick={() => void fetchModels()} title="The daemon sends one GET to the models list of the server, with the key if you typed one">
          Fetch models
        </button>
      </div>
      <Show when={fetchNote()}>
        <p class="muted small" style={{ margin: 0 }} role="status">
          {fetchNote()}
        </p>
      </Show>
      <div class="grid" style={{ "grid-template-columns": "repeat(auto-fill, minmax(min(100%, 180px), 1fr))" }}>
        <label>
          Default model
          <Show when={models().length} fallback={<input value={f().defaultModel} onInput={(e) => set({ defaultModel: e.currentTarget.value })} placeholder="the first model" />}>
            <select value={f().defaultModel} onChange={(e) => set({ defaultModel: e.currentTarget.value })}>
              <option value="">the first model</option>
              <For each={[...new Set([...models(), ...fetched()])]}>{(m) => <option value={m}>{m}</option>}</For>
            </select>
          </Show>
        </label>
        <label>
          Context window (tokens)
          <input inputmode="numeric" value={f().contextWindow} onInput={(e) => set({ contextWindow: e.currentTarget.value })} placeholder="131072 or 128k" />
        </label>
        <label>
          Max output tokens
          <input inputmode="numeric" value={f().maxOutputTokens} onInput={(e) => set({ maxOutputTokens: e.currentTarget.value })} placeholder="16384" />
        </label>
        <label>
          Concurrent streams
          <input inputmode="numeric" value={f().maxConcurrent} onInput={(e) => set({ maxConcurrent: e.currentTarget.value })} placeholder="no limit" />
        </label>
        <label>
          Status page (optional)
          <input type="url" value={f().statusUrl} onInput={(e) => set({ statusUrl: e.currentTarget.value })} placeholder="http://gpu.lan:3001/status" />
        </label>
      </div>
      <fieldset class="row" style={{ "flex-wrap": "wrap", border: "none", padding: 0 }}>
        <legend class="small">Harnesses that can use it ({f().api} style)</legend>
        <For each={STYLE_HARNESSES[f().api]}>
          {(h) => (
            <label class="row small">
              <input type="checkbox" checked={f().harnesses.includes(h)} onChange={(e) => set({ harnesses: styleHarnesses(f().api, e.currentTarget.checked ? [...f().harnesses, h] : f().harnesses.filter((y) => y !== h)) })} />
              {h}
            </label>
          )}
        </For>
      </fieldset>
      <p class="muted small" style={{ margin: 0 }}>
        Concurrent streams is how many agents on this subscription may run a turn at the same time; the work of the others waits. Copilot cannot use a self-hosted server. The key, if any, stays on the daemon.
      </p>
      <div>
        <button class="primary" type="submit" disabled={busy() || !f().name.trim() || !f().baseUrl.trim()}>
          Add server
        </button>
      </div>
    </form>
  );
}
