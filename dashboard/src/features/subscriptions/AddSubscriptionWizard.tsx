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
import { humanize } from "../../lib/format";
import { api, subscriptions } from "../../stores";
import {
  apiKeyBody,
  countdown,
  initialWizard,
  isDeviceFlow,
  isLoginPath,
  KEY_HELP,
  KEY_PROVIDERS,
  sessionToCancel,
  shouldPoll,
  takesCode,
  takesPlan,
  teardownLeft,
  wizardReduce,
  ZAI_DEFAULT_PLAN,
  ZAI_PLANS,
  type WizardAction,
  type WizardLogin,
  type WizardPath,
  type WizardSetup,
  type WizardState,
} from "./wizard";

export const PATHS: { path: WizardPath; title: string; detail: string }[] = [
  { path: "claude", title: "Claude Code", detail: "Anthropic plan. Runs claude auth login; paste the code back." },
  { path: "codex", title: "Codex", detail: "ChatGPT plan. Device code with codex login --device-auth." },
  { path: "copilot", title: "Copilot", detail: "GitHub plan. Device code with copilot login --device-code." },
  { path: "api_key", title: "API key", detail: "Anthropic, OpenAI, Google, OpenRouter, xAI, Groq, Z.ai (GLM), an OpenAI-compatible endpoint, or a GitHub PAT. pi uses API keys only." },
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
                <button onClick={() => send({ type: "pick", path: k.path })}>
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
          <a href={props.session.url} target="_blank" rel="noopener noreferrer" class="ellipsis">
            {props.session.url}
          </a>
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
  const [provider, setProvider] = createSignal(props.setup.provider ?? "openrouter");
  const [name, setName] = createSignal(props.setup.name ?? `${props.setup.provider ?? "openrouter"}-key`);
  const [key, setKey] = createSignal("");
  const [baseUrl, setBaseUrl] = createSignal("");
  const [plan, setPlan] = createSignal(ZAI_DEFAULT_PLAN);
  const [models, setModels] = createSignal("");
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  const fixedName = !!props.setup.name;

  const submit = async (e: Event) => {
    e.preventDefault();
    setBusy(true);
    try {
      const n = name().trim();
      await api.subscriptions.setApiKey(n, apiKeyBody({ provider: provider(), key: key(), plan: plan(), baseUrl: baseUrl(), models: models() }));
      setKey("");
      props.onSaved(n);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };

  const env = () => KEY_PROVIDERS.find((p) => p.id === provider())?.env;

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
      <label>
        {provider() === "github" ? "Fine-grained PAT (Copilot Requests permission)" : "API key"}
        <input type="password" value={key()} onInput={(e) => setKey(e.currentTarget.value)} autocomplete="off" required />
      </label>
      <Show when={takesPlan(provider())}>
        <label>
          Plan
          <select value={plan()} onChange={(e) => setPlan(e.currentTarget.value)}>
            <For each={ZAI_PLANS}>{(p) => <option value={p.id}>{p.label}</option>}</For>
          </select>
        </label>
      </Show>
      <Show when={KEY_HELP[provider()]}>
        <p class="muted small" style={{ margin: 0 }}>
          {KEY_HELP[provider()]}
        </p>
      </Show>
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
      <p class="muted small" style={{ margin: 0 }}>
        The daemon stores the key with mode 0600 and gives it to the agents on this subscription as <code>{env()}</code>. No endpoint returns it, and this page never shows it again.
      </p>
      <div>
        <button class="primary" type="submit" disabled={busy() || !key() || !name().trim()}>
          Save key
        </button>
      </div>
    </form>
  );
}

function DefineForm(props: { onBack: () => void; onSaved: (name: string) => void }) {
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
                  {s().windows.map((w) => `${w.window} ${Math.round(w.used_pct)}%`).join(", ")}
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
