import { InstanceAnchor } from "../../instance/navigation";
import { useInstance } from "../../instance";
// Settings, Subscriptions tab (PLAN §16.1, docs/api.md §16): every plan
// with its login and probe status, the logins the daemon found with no
// definition, and the actions: verify, probe, re-login, logout, delete. Above them, the harness
// versions against their pins (./HarnessVersions.tsx).

import { createSignal, For, onMount, Show } from "solid-js";
import { ConfirmButton, Empty, ErrorBanner, PageHead, ProviderLogo, useNow } from "../../components/ui";
import { isLocalOnly, LOGIN_CHIP } from "../../components/UsageTile";
import type { DetectedLogin, Subscription } from "../../api/types";
import { AssignedAgentsChip } from "../../components/AssignedAgentsChip";
import { ago, balanceText, humanize, orderWindows, subscriptionTitle, timeUntil, windowLabel, windowValue } from "../../lib/format";

import { AddSubscriptionWizard } from "./AddSubscriptionWizard";
import { HarnessVersions } from "./HarnessVersions";
import { isSelfHosted, reloginFor, selfHostedSummary, stopAllText, streamsText, wizardFromQuery, type WizardPath, type WizardSetup } from "./wizard";
import { useSearchParams } from "@solidjs/router";
import { A } from "../../instance/navigation";
import { accountText, defaultInfo } from "./account";

/** The probe chip: `ok` with the last success, `failed` with why, or local only. */
export function probeChip(s: Subscription, now = Date.now()): { cls: string; text: string; title?: string } {
  if (s.no_quota) return { cls: "", text: "no quota", title: "A self-hosted server has no quota: the usage counts only." };
  if (s.probe_status === "ok") return { cls: "ok", text: `probe ok${s.last_success_at ? ` · ${ago(s.last_success_at, now)}` : ""}` };
  if (s.probe_status === "failed") return { cls: "warn", text: "probe failed", title: s.probe_error };
  return { cls: "", text: s.probe ? "probe not run" : "local estimate only" };
}

/** A harness with a logout command (pi has none). A self-hosted server has no login. */
const canLogout = (s: Subscription) => !s.has_api_key && !isSelfHosted(s.provider) && s.harnesses.some((h) => h !== "pi");

const suggestName = (d: DetectedLogin) => d.suggested.name || `${d.pi_provider ?? d.provider}-${d.harness}`;

function CloudBudget(props: { subscription: Subscription; onSaved: () => Promise<void> }) {
  const { api } = useInstance();
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  return <form onSubmit={async (e) => {
    e.preventDefault();
    const data = new FormData(e.currentTarget);
    const text = String(data.get("budget") ?? "").trim();
    setBusy(true);
    setError(null);
    try {
      await api.subscriptions.setCloud(props.subscription.name, { ...props.subscription.cloud, budget_usd: text ? Number(text) : null });
      await props.onSaved();
    } catch (err) { setError(err); } finally { setBusy(false); }
  }}>
    <label>Monthly budget (USD)
      <input name="budget" type="number" min="1" max="4294967295" step="1" value={props.subscription.cloud?.budget_usd ?? ""} disabled={props.subscription.source !== "api" || busy()} />
    </label>
    <span class="muted small">Blank clears it. Sends mail at 80% and 100%, never holds agents.</span>
    <button type="submit" disabled={props.subscription.source !== "api" || busy()}>Save budget</button>
    <ErrorBanner error={error()} />
  </form>;
}

export default function SubscriptionsPage() {
  const { subscriptions, api } = useInstance();
  const now = useNow(30_000);
  const [wizard, setWizard] = createSignal<{ path?: WizardPath; setup?: WizardSetup } | null>(null);
  const [error, setError] = createSignal<unknown>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal<string | null>(null);

  const [params, setParams] = useSearchParams<{ add?: string }>();
  onMount(() => {
    void subscriptions.detect();
    // `?add=api_key` from the agent form: open the wizard at that step.
    const w = wizardFromQuery(params.add);
    if (w) setWizard(w);
  });

  const act = async (name: string, fn: () => Promise<string | void>) => {
    setBusy(name);
    try {
      const msg = await fn();
      await subscriptions.refresh();
      setError(null);
      setNotice(msg || null);
    } catch (e) {
      setError(e);
      setNotice(null);
    } finally {
      setBusy(null);
    }
  };

  const createFrom = (d: DetectedLogin) =>
    act(`detect:${suggestName(d)}`, async () => {
      const s = await api.subscriptions.create({ ...d.suggested, name: suggestName(d) });
      await subscriptions.detect();
      return `Created ${s.name}. Run a probe or verify it from its row.`;
    });

  return (
    <>
      <PageHead title="Subscriptions" level={2}>
        <button onClick={() => void Promise.all([subscriptions.refresh(), subscriptions.detect()])}>Refresh</button>
        <button class="primary" onClick={() => setWizard({})}>
          Add subscription
        </button>
      </PageHead>
      <ErrorBanner error={error() ?? subscriptions.error()} onDismiss={() => setError(null)} />
      <Show when={notice()}>
        <p class="notice small">{notice()}</p>
      </Show>

      <HarnessVersions />

      <Show when={subscriptions.detected().length}>
        <div class="notice stack" style={{ gap: "6px", "margin-bottom": "14px" }} aria-label="Logins with no subscription">
          <For each={subscriptions.detected()}>
            {(d) => (
              <div class="spread">
                <span>
                  Found a <strong>{d.harness}</strong> login for <strong>{d.pi_provider ?? d.provider}</strong>
                  {d.plan ? ` (${d.plan})` : ""} with no subscription.
                  <Show when={d.detail}>
                    {" "}
                    <span class="muted small">{d.detail}</span>
                  </Show>
                </span>
                <button class="small primary" disabled={busy() === `detect:${suggestName(d)}`} onClick={() => createFrom(d)}>
                  Create {suggestName(d)}
                </button>
              </div>
            )}
          </For>
        </div>
      </Show>

      <div class="stack">
        <For each={subscriptions.list()} fallback={<Empty>{subscriptions.loaded() ? "No subscriptions yet. Add one, or create one from a login the daemon found." : "Loading…"}</Empty>}>
          {(s) => {
            const probe = () => probeChip(s, now());
            const def = () => defaultInfo(subscriptions.list(), s);
            return (
              <div class="card-box spread" style={{ "align-items": "flex-start" }}>
                <div class="row" style={{ "align-items": "flex-start", gap: "12px", "min-width": 0 }}>
                  <ProviderLogo provider={s.provider} />
                  <div style={{ "min-width": 0 }}>
                    <div class="row" style={{ "flex-wrap": "wrap" }}>
                      <strong>{subscriptionTitle(s)}</strong>
                      <span class="muted small mono" title="The subscription name">
                        {s.name}
                      </span>
                      <AssignedAgentsChip sub={s} />
                      <Show when={def().isDefault}>
                        <span class="chip ok" title={`Agents of ${s.account?.harness} with no subscription run on this account`}>
                          default
                        </span>
                      </Show>
                      <Show when={def().noDefault}>
                        <span class="chip warn" title={`No ${s.account?.harness} account is the default: a ${s.account?.harness} agent with no subscription cannot start. Make one the default.`}>
                          no default
                        </span>
                      </Show>
                      <span class={`chip ${LOGIN_CHIP[s.login_status]}`} title={s.verified_at ? `Verified ${ago(s.verified_at, now())}` : "Not verified yet"}>
                        {humanize(s.login_status)}
                      </span>
                      <span class={`chip ${probe().cls}`} title={probe().title}>
                        {probe().text}
                      </span>
                      <Show when={s.source === "file"}>
                        <span class="chip" title={`subscriptions/${s.name}.toml`}>
                          file
                        </span>
                      </Show>
                      <Show when={s.has_api_key}>
                        <span class="chip info" title="The key is stored on the daemon and never shown">
                          API key{s.env_var ? ` · ${s.env_var}` : ""}
                        </span>
                      </Show>
                      <Show when={s.held_agents > 0}>
                        <span class="chip bad">{s.held_agents} agents held</span>
                      </Show>
                      <Show when={s.enabled === false}>
                        <span class="chip warn" title="No agent on it starts or wakes, and no new agent takes it. Agents that run go on until they stop.">
                          disabled
                        </span>
                      </Show>
                      <Show when={selfHostedSummary(s)}>
                        {(t) => (
                          <span class="chip info" title={s.selfhosted?.base_url}>
                            {t()}
                          </span>
                        )}
                      </Show>
                      <Show when={streamsText(s)}>
                        {(t) => <span class={`chip ${(s.streams?.waiting ?? 0) > 0 ? "warn" : ""}`} title="Agents that run a turn now, of the stream limit. The work of the others waits.">{t()}</span>}
                      </Show>
                    </div>
                    <Show when={s.selfhosted}>
                      {(sh) => (
                        <div class="muted small">
                          <span class="mono">{sh().base_url}</span>
                          {sh().models?.length ? ` · models: ${sh().models!.join(", ")}` : ""}
                          {sh().default_model ? ` · default ${sh().default_model}` : ""}
                          {sh().max_output_tokens ? ` · max output ${sh().max_output_tokens}` : ""}
                        </div>
                      )}
                    </Show>
                    <div class="muted small">
                      {s.plan ?? (s.selfhosted ? "no plan" : "plan not known")} · {s.provider} · harnesses: {s.harnesses.join(", ") || "none"} · probe: {s.probe ?? "none"} · alert at {s.alert_at}%
                    </div>
                    <div class="muted small">
                      Verified {s.verified_at ? ago(s.verified_at, now()) : "never"} · last probe {s.last_probe_at ? ago(s.last_probe_at, now()) : "never"} · last success{" "}
                      {s.last_success_at ? ago(s.last_success_at, now()) : "never"}
                      <Show when={isLocalOnly(s)}> · local estimate only</Show>
                    </div>
                    <Show when={s.account}>
                      {(acct) => <div class="small">{accountText(acct())}</div>}
                    </Show>
                    <div class="small">
                      Agents on it:{" "}
                      <Show when={s.agents?.length} fallback={<span class="muted">none</span>}>
                        <For each={s.agents}>
                          {(a, i) => (
                            <>
                              {i() > 0 ? ", " : ""}
                              <A href={`/agents/${encodeURIComponent(a.id)}`}>
                                {a.project}/{a.name}
                              </A>
                            </>
                          )}
                        </For>
                      </Show>
                    </div>
                    <Show when={s.session_period}>
                      <div class="small">Sessions: {s.session_period?.since} – {s.session_period?.until}</div>
                      <For each={s.sessions ?? []}>{(session) => (
                        <div class="small">
                          <A href={`/sessions/${encodeURIComponent(session.id)}`}>{session.project}/{session.agent}</A>
                          {" · "}{session.harness} · {session.model ?? "model unknown"} · {session.card ?? "no card"} · {session.started_at}
                          {" · "}{session.input_tokens ?? "?"} in / {session.output_tokens ?? "?"} out
                        </div>
                      )}</For>
                    </Show>
                    <Show when={s.probe_status === "failed" && s.probe_error}>
                      <div class="small" style={{ color: "var(--warn)" }}>
                        Probe error: {s.probe_error}
                      </div>
                    </Show>
                    <Show when={s.fallback_spend_usd != null && !s.windows.some((w) => w.window === "budget")}>
                      <div class="muted small">Local monthly spend: ${s.fallback_spend_usd?.toFixed(2)} USD</div>
                    </Show>
                    <Show when={s.month_spend_usd !== undefined}>
                      <div class="small">This month (UTC): ${s.month_spend_usd?.toFixed(2)} USD · local estimate</div>
                    </Show>
                    <Show when={["bedrock", "gcp", "foundry"].includes(s.provider)}>
                      <CloudBudget subscription={s} onSaved={() => subscriptions.refresh()} />
                    </Show>
                    <Show when={s.windows.length}>
                      <div class="muted small">
                        {orderWindows(s.windows).map((w) => `${windowLabel(w.window)} ${w.balance ? balanceText(w.balance) : `${Math.round(w.used_pct)}%`}${w.balance ? "" : w.resets_at ? ` (resets in ${timeUntil(w.resets_at, now())})` : ""}`).join(" · ")}
                      </div>
                    </Show>
                  </div>
                </div>
                <div class="row" style={{ "flex-wrap": "wrap", "justify-content": "flex-end" }}>
                  <Show when={s.status_url}>
                    {(u) => (
                      <InstanceAnchor class="btn small" href={u()} target="_blank" rel="noopener noreferrer" title={`Open ${u()} in a new tab`}>
                        Status page
                      </InstanceAnchor>
                    )}
                  </Show>
                  <Show
                    when={s.enabled === false}
                    fallback={
                      <button
                        class="small"
                        disabled={busy() === s.name}
                        title="No agent on it starts or wakes. Agents that run go on until they stop."
                        onClick={() =>
                          act(s.name, async () => {
                            await api.subscriptions.disable(s.name);
                            return `${s.name} is disabled. Agents that run go on; none starts or wakes.`;
                          })
                        }
                      >
                        Disable
                      </button>
                    }
                  >
                    <button
                      class="small"
                      disabled={busy() === s.name}
                      onClick={() =>
                        act(s.name, async () => {
                          await api.subscriptions.enable(s.name);
                          return `${s.name} is enabled. Agents with work wake now.`;
                        })
                      }
                    >
                      Enable
                    </button>
                  </Show>
                  <ConfirmButton
                    class="small danger"
                    label="Stop all now"
                    confirmLabel="Yes, stop all"
                    disabled={busy() === s.name}
                    onConfirm={() =>
                      act(s.name, async () => stopAllText(s.name, await api.subscriptions.stopAll(s.name), false))
                    }
                  />
                  <Show when={s.enabled !== false}>
                    <ConfirmButton
                      class="small danger"
                      label="Disable and stop all"
                      confirmLabel="Yes, disable and stop all"
                      disabled={busy() === s.name}
                      onConfirm={() =>
                        act(s.name, async () => stopAllText(s.name, await api.subscriptions.stopAll(s.name, { disable: true }), true))
                      }
                    />
                  </Show>
                  <button
                    class="small"
                    disabled={busy() === s.name}
                    onClick={() =>
                      act(s.name, async () => {
                        const r = await api.subscriptions.verify(s.name);
                        return `${s.name}: ${r.status.logged_in ? "logged in" : "not logged in"}${r.status.detail ? ` (${r.status.detail})` : ""}.`;
                      })
                    }
                  >
                    Verify now
                  </button>
                  <Show when={s.probe}>
                    <button
                      class="small"
                      disabled={busy() === s.name}
                      onClick={() =>
                        act(s.name, async () => {
                          const r = await api.subscriptions.probe(s.name);
                          return r.probe_status === "ok" ? `${s.name}: probe ok, ${orderWindows(r.windows).map((w) => `${windowLabel(w.window)} ${windowValue(w)}`).join(", ") || "no windows"}.` : `${s.name}: probe ${r.probe_status}${r.error ? `: ${r.error}` : ""}.`;
                        })
                      }
                    >
                      Probe now
                    </button>
                  </Show>
                  <Show when={def().canMakeDefault}>
                    <button
                      class="small"
                      disabled={busy() === s.name}
                      title={`Agents of ${s.account?.harness} with no subscription run on the default account`}
                      onClick={() =>
                        act(s.name, async () => {
                          await api.subscriptions.makeDefault(s.name);
                          return `${s.name} is the default ${s.account?.harness} account.`;
                        })
                      }
                    >
                      Make default
                    </button>
                  </Show>
                  <button class="small" onClick={() => setWizard(reloginFor(s))}>
                    {s.has_api_key ? "Replace key" : isSelfHosted(s.provider) ? "Add key" : "Re-login"}
                  </button>
                  <Show when={canLogout(s)}>
                    <ConfirmButton class="small danger" label="Logout" disabled={busy() === s.name || s.login_status === "missing"} onConfirm={() => act(s.name, async () => void (await api.subscriptions.logout(s.name)))} />
                  </Show>
                  <Show when={s.source === "api"}>
                    <ConfirmButton
                      class="small danger"
                      label="Delete"
                      disabled={busy() === s.name}
                      onConfirm={() =>
                        act(s.name, async () => {
                          await api.subscriptions.remove(s.name);
                          subscriptions.drop(s.name);
                          return `Deleted ${s.name}.`;
                        })
                      }
                    />
                  </Show>
                </div>
              </div>
            );
          }}
        </For>
      </div>

      <Show when={wizard()}>
        {(w) => (
          <AddSubscriptionWizard
            path={w().path}
            setup={w().setup}
            onClose={() => {
              setWizard(null);
              if (params.add) setParams({ add: undefined });
              void subscriptions.refresh();
            }}
          />
        )}
      </Show>
    </>
  );
}
