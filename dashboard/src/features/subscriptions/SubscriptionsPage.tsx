// Subscriptions (PLAN §16.1, docs/api.md §16): every plan with its login and
// probe status, the logins the daemon found with no definition, and the
// actions: verify, probe, re-login, logout, delete. Above them, the harness
// versions against their pins (./HarnessVersions.tsx).

import { createSignal, For, onMount, Show } from "solid-js";
import { ConfirmButton, Empty, ErrorBanner, PageHead, ProviderLogo, useNow } from "../../components/ui";
import { isLocalOnly, LOGIN_CHIP } from "../../components/UsageTile";
import type { DetectedLogin, Subscription } from "../../api/types";
import { ago, balanceText, humanize, timeUntil, windowValue } from "../../lib/format";
import { api, subscriptions } from "../../stores";
import { AddSubscriptionWizard } from "./AddSubscriptionWizard";
import { HarnessVersions } from "./HarnessVersions";
import { reloginFor, wizardFromQuery, type WizardPath, type WizardSetup } from "./wizard";
import { useSearchParams } from "@solidjs/router";

/** The probe chip: `ok` with the last success, `failed` with why, or local only. */
export function probeChip(s: Subscription, now = Date.now()): { cls: string; text: string; title?: string } {
  if (s.probe_status === "ok") return { cls: "ok", text: `probe ok${s.last_success_at ? ` · ${ago(s.last_success_at, now)}` : ""}` };
  if (s.probe_status === "failed") return { cls: "warn", text: "probe failed", title: s.probe_error };
  return { cls: "", text: s.probe ? "probe not run" : "local estimate only" };
}

/** A harness with a logout command (pi has none). */
const canLogout = (s: Subscription) => !s.has_api_key && s.harnesses.some((h) => h !== "pi");

const suggestName = (d: DetectedLogin) => d.suggested.name || `${d.pi_provider ?? d.provider}-${d.harness}`;

export default function SubscriptionsPage() {
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
      <PageHead title="Subscriptions">
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
            return (
              <div class="card-box spread" style={{ "align-items": "flex-start" }}>
                <div class="row" style={{ "align-items": "flex-start", gap: "12px", "min-width": 0 }}>
                  <ProviderLogo provider={s.provider} />
                  <div style={{ "min-width": 0 }}>
                    <div class="row" style={{ "flex-wrap": "wrap" }}>
                      <strong>{s.name}</strong>
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
                    </div>
                    <div class="muted small">
                      {s.plan ?? "plan not known"} · {s.provider} · harnesses: {s.harnesses.join(", ") || "none"} · probe: {s.probe ?? "none"} · alert at {s.alert_at}%
                    </div>
                    <div class="muted small">
                      Verified {s.verified_at ? ago(s.verified_at, now()) : "never"} · last probe {s.last_probe_at ? ago(s.last_probe_at, now()) : "never"} · last success{" "}
                      {s.last_success_at ? ago(s.last_success_at, now()) : "never"}
                      <Show when={isLocalOnly(s)}> · local estimate only</Show>
                    </div>
                    <Show when={s.probe_status === "failed" && s.probe_error}>
                      <div class="small" style={{ color: "var(--warn)" }}>
                        Probe error: {s.probe_error}
                      </div>
                    </Show>
                    <Show when={s.windows.length}>
                      <div class="muted small">
                        {s.windows.map((w) => `${w.window} ${w.balance ? balanceText(w.balance) : `${Math.round(w.used_pct)}%`}${w.balance ? "" : w.resets_at ? ` (resets in ${timeUntil(w.resets_at, now())})` : ""}`).join(" · ")}
                      </div>
                    </Show>
                  </div>
                </div>
                <div class="row" style={{ "flex-wrap": "wrap", "justify-content": "flex-end" }}>
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
                          return r.probe_status === "ok" ? `${s.name}: probe ok, ${r.windows.map((w) => `${w.window} ${windowValue(w)}`).join(", ") || "no windows"}.` : `${s.name}: probe ${r.probe_status}${r.error ? `: ${r.error}` : ""}.`;
                        })
                      }
                    >
                      Probe now
                    </button>
                  </Show>
                  <button class="small" onClick={() => setWizard(reloginFor(s))}>
                    {s.has_api_key ? "Replace key" : "Re-login"}
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
